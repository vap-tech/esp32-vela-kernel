//! Cross-subsystem object lifecycle and IPC/capability/memory orchestration.
//!
//! Individual tables (`Processes`, `Ipc`, `CallTable`, `RegionAllocator`,
//! `Scheduler`) stay self-contained; multi-table state transitions live here so
//! revocation, endpoint teardown, RPC cycle checks, shared-memory unmapping,
//! and process reaping are enforced and host-tested without SoC or raw user
//! pointer dependencies.

use crate::{
    call::CallTable,
    capability::{self as cap, Authority, Capability, Object},
    errno::Errno,
    id::{Handle, Pid, ProcSlot, ThreadSlot, Ticks, Tid},
    ipc::{self, Ipc, Message},
    memory::RegionAllocator,
    process::{self, AddressSpace, Lifecycle, Processes},
    sched::{Scheduler, TASKS},
    task::{Context, Region, State, Wait},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingReply {
    pub client: ThreadSlot,
    pub target: ProcSlot,
    pub pointer: usize,
    pub capacity: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReapedProcess {
    pub slot: ProcSlot,
    pub pid: Pid,
    pub first_woken: Option<Tid>,
}

/// Create an IPC endpoint owned by `owner` and install its root capability,
/// rolling back the endpoint slot if the process capability table is full.
pub fn create_endpoint(
    ipc: &mut Ipc,
    processes: &mut Processes,
    owner: ProcSlot,
) -> Result<Handle, Errno> {
    let object = ipc.create(owner)?;
    match processes.slot_mut(owner).capabilities.install(Capability {
        object,
        rights: cap::SEND | cap::RECEIVE | cap::GRANT | cap::CONTROL,
    }) {
        Ok(handle) => Ok(handle),
        Err(e) => {
            let ep = ipc.resolve(object)?;
            ipc.close(ep);
            Err(e)
        }
    }
}

/// Grant a one-level delegated capability from `owner` to `peer`.
/// `GRANT` and `CONTROL` are never delegable.
pub fn grant(
    processes: &mut Processes,
    owner: ProcSlot,
    handle: Handle,
    peer: ProcSlot,
    rights: u16,
) -> Result<Handle, Errno> {
    let source = processes.slot(owner).capabilities.get(handle, cap::GRANT)?;
    if rights == 0 || rights & !source.rights != 0 || rights & (cap::CONTROL | cap::GRANT) != 0 {
        return Err(Errno::Eperm);
    }
    processes.slot_mut(peer).capabilities.install(Capability {
        object: source.object,
        rights,
    })
}

/// Grant a capability (and map into `peer`'s address space if it is a memory
/// object), rolling back the granted handle if address-space insertion fails.
pub fn grant_or_map(
    processes: &mut Processes,
    owner: ProcSlot,
    handle: Handle,
    peer_pid: Pid,
    rights: u16,
    require_memory: bool,
) -> Result<Handle, Errno> {
    let peer = match processes.find_alive(peer_pid) {
        Some(p) if p != owner => p,
        _ => return Err(Errno::Einval),
    };
    let source = processes.slot(owner).capabilities.get(handle, cap::GRANT)?;
    if let Object::Memory { region, .. } = source.object {
        if rights & !(cap::READ | cap::WRITE) != 0 || rights & cap::READ == 0 {
            return Err(Errno::Einval);
        }
        let granted = grant(processes, owner, handle, peer, rights)?;
        let mut perms = process::READ;
        if rights & cap::WRITE != 0 {
            perms |= process::WRITE;
        }
        if let Err(e) = processes.slot_mut(peer).memory.insert(region, perms) {
            processes
                .slot_mut(peer)
                .capabilities
                .close(granted)
                .unwrap();
            return Err(e);
        }
        Ok(granted)
    } else if require_memory {
        Err(Errno::Einval)
    } else {
        grant(processes, owner, handle, peer, rights)
    }
}

/// Verify that `process` holds `handle` naming `Object::Authority(expected)` with `rights`.
pub fn require_authority(
    processes: &Processes,
    process: ProcSlot,
    handle: Handle,
    expected: Authority,
    rights: u16,
) -> Result<(), Errno> {
    let capability = processes.slot(process).capabilities.get(handle, 0)?;
    if capability.object != Object::Authority(expected) {
        return Err(Errno::Ebadf);
    }
    if capability.rights & rights != rights {
        return Err(Errno::Eperm);
    }
    Ok(())
}

/// Verify that `process` holds `handle` naming `expected` event object with `rights`.
pub fn require_event(
    processes: &Processes,
    process: ProcSlot,
    handle: Handle,
    expected: Object,
    rights: u16,
) -> Result<(), Errno> {
    let capability = processes.slot(process).capabilities.get(handle, 0)?;
    if capability.object != expected {
        return Err(Errno::Ebadf);
    }
    if capability.rights & rights != rights {
        return Err(Errno::Eperm);
    }
    Ok(())
}

/// Validate `handle` and `rights` in `process` and resolve the live IPC endpoint.
pub fn resolve_endpoint(
    processes: &Processes,
    ipc: &Ipc,
    process: ProcSlot,
    handle: Handle,
    rights: u16,
) -> Result<usize, Errno> {
    let capability = processes.slot(process).capabilities.get(handle, rights)?;
    ipc.resolve(capability.object)
}

/// Revoke `object` across all processes (except `keep_owner`, if specified).
/// For `Object::Memory`, unmaps the region from affected address spaces and
/// wakes any blocked `Wait::Receive` targeting that region with `-EFAULT`.
pub fn invalidate_object<C: Context>(
    processes: &mut Processes,
    scheduler: &mut Scheduler<C>,
    object: Object,
    keep_owner: Option<ProcSlot>,
) {
    for idx in 0..processes.slots.len() {
        let process = ProcSlot(idx);
        if Some(process) == keep_owner {
            continue;
        }
        processes.slot_mut(process).capabilities.revoke(object);
        if let Object::Memory { region, .. } = object
            && processes
                .slot(process)
                .memory
                .maps
                .iter()
                .any(|m| m.region == region && m.region.size != 0)
        {
            processes.slot_mut(process).memory.remove(region).unwrap();
            for thread_idx in 0..TASKS {
                let thread = ThreadSlot(thread_idx);
                if scheduler.task(thread).process == process
                    && scheduler.task(thread).state == State::Blocked
                    && let Wait::Receive {
                        pointer, capacity, ..
                    } = scheduler.task(thread).wait
                    && region.contains(pointer, capacity)
                {
                    scheduler.wake_with(thread, Errno::Efault.to_abi());
                }
            }
        }
    }
}

/// Complete any threads blocked in `Wait::Receive` on `endpoint`.
/// `deliver` copies message payload bytes into the receiver's user buffer.
/// Returns the `Tid` of the first woken thread, if any.
pub fn complete_receivers<C: Context>(
    processes: &Processes,
    ipc: &mut Ipc,
    calls: &mut CallTable,
    scheduler: &mut Scheduler<C>,
    endpoint: usize,
    closed: bool,
    mut deliver: impl FnMut(&AddressSpace, usize, usize, &[u8]) -> Result<(), Errno>,
) -> Option<Tid> {
    let mut first_woken = None;
    for i in 0..TASKS {
        let thread = ThreadSlot(i);
        let task = *scheduler.task(thread);
        if task.state != State::Blocked {
            continue;
        }
        let Wait::Receive {
            endpoint: waiting,
            handle,
            pointer,
            capacity,
        } = task.wait
        else {
            continue;
        };
        if waiting != endpoint {
            continue;
        }
        let result = if closed
            || resolve_endpoint(processes, ipc, task.process, handle, ipc::RECEIVE).is_err()
        {
            Err(Errno::Epipe)
        } else {
            ipc.receive(endpoint, capacity)
        };
        let value = match result {
            Ok(None) => continue,
            Ok(Some(message)) => {
                calls.bind_reply(scheduler, message.reply_token, thread);
                scheduler
                    .task_mut(thread)
                    .context
                    .set_return2(message.reply_token);
                match deliver(
                    &processes.slot(task.process).memory,
                    pointer,
                    capacity,
                    &message.data[..message.length],
                ) {
                    Ok(()) => message.length as u32,
                    Err(e) => e.to_abi(),
                }
            }
            Err(error) => error.to_abi(),
        };
        scheduler.wake_with(thread, value);
        scheduler.prefer(thread);
        if first_woken.is_none() {
            first_woken = Some(task.id);
        }
    }
    first_woken
}

/// Resolve the RPC endpoint and select an initial server thread for priority
/// inheritance, rejecting self-process RPC, dead server processes, and
/// transitive `Wait::Reply` cycles.
pub fn find_rpc_server<C: Copy>(
    processes: &Processes,
    ipc: &Ipc,
    scheduler: &Scheduler<C>,
    current: ProcSlot,
    client: ThreadSlot,
    handle: Handle,
) -> Result<(usize, ThreadSlot), Errno> {
    let ep = resolve_endpoint(processes, ipc, current, handle, cap::SEND)?;
    let server_process = ipc.owner(ep).ok_or(Errno::Epipe)?;
    // No self-process RPC: avoid an unbounded dependency on caller-owned locks.
    if server_process == current {
        return Err(Errno::Edeadlk);
    }
    let server = scheduler
        .tasks
        .iter()
        .position(|t| t.process == server_process && t.state != State::Terminated)
        .map(ThreadSlot)
        .ok_or(Errno::Epipe)?;
    let mut dependency = server;
    for _ in 0..TASKS {
        if dependency == client {
            return Err(Errno::Edeadlk);
        }
        if scheduler.task(dependency).state == State::Blocked
            && let Wait::Reply { server: next, .. } = scheduler.task(dependency).wait
        {
            dependency = next;
        } else {
            break;
        }
    }
    Ok((ep, server))
}

/// Enqueue a prepared RPC request, activate its call-table entry, block the
/// current client thread on `Wait::Reply`, wake waiting receivers, and recompute
/// transitive priority inheritance.
#[allow(clippy::too_many_arguments)]
pub fn commit_ipc_call<C: Context>(
    processes: &Processes,
    ipc: &mut Ipc,
    calls: &mut CallTable,
    scheduler: &mut Scheduler<C>,
    ep: usize,
    server: ThreadSlot,
    generation: u32,
    message: Message,
    reply_pointer: usize,
    reply_capacity: usize,
    deadline: Option<Ticks>,
    deliver: impl FnMut(&AddressSpace, usize, usize, &[u8]) -> Result<(), Errno>,
) -> Result<Option<Tid>, Errno> {
    let client = scheduler.current;
    let token = message.reply_token;
    ipc.send(ep, message)?;
    calls.activate(client, generation, ep);
    scheduler.block_opt(
        Wait::Reply {
            token,
            pointer: reply_pointer,
            capacity: reply_capacity,
            server,
        },
        deadline,
    );
    let woken = complete_receivers(processes, ipc, calls, scheduler, ep, false, deliver);
    scheduler.inherit();
    Ok(woken)
}

/// Validate a reply token, caller ownership, client wait state, and payload size
/// before copying reply bytes between user buffers.
pub fn prepare_ipc_reply<C: Copy>(
    calls: &CallTable,
    scheduler: &Scheduler<C>,
    current: ProcSlot,
    token: u32,
    length: usize,
) -> Result<PendingReply, Errno> {
    let (client, server) = calls.lookup_for_reply(token)?;
    if scheduler.task(server).process != current {
        return Err(Errno::Eperm);
    }
    let task = scheduler.task(client);
    if task.state != State::Blocked {
        return Err(Errno::Epipe);
    }
    let Wait::Reply {
        token: wait_token,
        pointer,
        capacity,
        ..
    } = task.wait
    else {
        return Err(Errno::Epipe);
    };
    if wait_token != token {
        return Err(Errno::Epipe);
    }
    if length > capacity || length > ipc::MESSAGE_BYTES {
        return Err(Errno::Emsgsize);
    }
    Ok(PendingReply {
        client,
        target: task.process,
        pointer,
        capacity,
    })
}

/// Complete an RPC reply after payload bytes have been copied into the client.
pub fn finish_ipc_reply<C: Context>(
    calls: &mut CallTable,
    scheduler: &mut Scheduler<C>,
    client: ThreadSlot,
    length: usize,
) {
    calls.deactivate(client);
    scheduler.wake_with(client, length as u32);
    scheduler.inherit();
    scheduler.prefer(client);
}

/// Revoke all delegated capabilities to the object referenced by `handle` while
/// retaining `current`'s own root capability.
pub fn revoke_capability<C: Context>(
    processes: &mut Processes,
    ipc: &mut Ipc,
    calls: &mut CallTable,
    scheduler: &mut Scheduler<C>,
    current: ProcSlot,
    handle: Handle,
    deliver: impl FnMut(&AddressSpace, usize, usize, &[u8]) -> Result<(), Errno>,
) -> Result<Option<Tid>, Errno> {
    let c = processes
        .slot(current)
        .capabilities
        .get(handle, cap::CONTROL)?;
    invalidate_object(processes, scheduler, c.object, Some(current));
    let mut woken = None;
    if let Object::Endpoint { index, .. } = c.object {
        calls.fail_where(scheduler, Errno::Epipe, |call, task| {
            call.endpoint() == index && task.process != current
        });
        woken = complete_receivers(processes, ipc, calls, scheduler, index, false, deliver);
    }
    Ok(woken)
}

/// Close `handle` on an endpoint. Closing a `CONTROL` capability destroys the
/// endpoint globally and wakes all callers and receivers with `-EPIPE`; closing
/// a delegated capability drops only the local handle.
pub fn close_endpoint<C: Context>(
    processes: &mut Processes,
    ipc: &mut Ipc,
    calls: &mut CallTable,
    scheduler: &mut Scheduler<C>,
    current: ProcSlot,
    handle: Handle,
    deliver: impl FnMut(&AddressSpace, usize, usize, &[u8]) -> Result<(), Errno>,
) -> Result<Option<Tid>, Errno> {
    let capability = processes.slot(current).capabilities.get(handle, 0)?;
    let ep = ipc.resolve(capability.object)?;
    let woken = if capability.rights & cap::CONTROL != 0 {
        calls.fail_for_endpoint(scheduler, ep, Errno::Epipe);
        ipc.close(ep);
        invalidate_object(processes, scheduler, capability.object, None);
        complete_receivers(processes, ipc, calls, scheduler, ep, true, deliver)
    } else {
        processes
            .slot_mut(current)
            .capabilities
            .close(handle)
            .unwrap();
        complete_receivers(processes, ipc, calls, scheduler, ep, false, deliver)
    };
    Ok(woken)
}

/// Look up `current`'s root `CONTROL` capability for a mapped memory region by base address.
pub fn memory_capability(
    processes: &Processes,
    current: ProcSlot,
    base: usize,
) -> Result<Handle, Errno> {
    let region = processes
        .slot(current)
        .memory
        .maps
        .iter()
        .find(|m| m.region.size != 0 && m.region.base == base)
        .map(|m| m.region)
        .ok_or(Errno::Ebadf)?;
    processes
        .slot(current)
        .capabilities
        .find(
            Object::Memory {
                owner: processes.slot(current).id,
                region,
            },
            cap::CONTROL,
        )
        .ok_or(Errno::Ebadf)
}

/// Allocate a NAPOT SRAM region for `current`, map it `READ | WRITE`, and
/// install its root capability, rolling back cleanly on any table limit error.
pub fn alloc_memory(
    processes: &mut Processes,
    allocator: &mut RegionAllocator,
    current: ProcSlot,
    requested: usize,
) -> Result<Region, Errno> {
    let owner = processes.slot(current).id;
    let region = allocator.allocate(owner, requested)?;
    if let Err(e) = processes
        .slot_mut(current)
        .memory
        .insert(region, process::READ | process::WRITE)
    {
        allocator.free(owner, region).unwrap();
        return Err(e);
    }
    if let Err(e) = processes
        .slot_mut(current)
        .capabilities
        .install(Capability {
            object: Object::Memory { owner, region },
            rights: cap::READ | cap::WRITE | cap::GRANT | cap::CONTROL,
        })
    {
        processes.slot_mut(current).memory.remove(region).unwrap();
        allocator.free(owner, region).unwrap();
        return Err(e);
    }
    Ok(region)
}

/// Free an owned memory region if no live thread stack resides inside it,
/// revoking all capabilities/mappings and waking blocked receivers with `-EFAULT`.
pub fn free_memory<C: Context>(
    processes: &mut Processes,
    allocator: &mut RegionAllocator,
    scheduler: &mut Scheduler<C>,
    current: ProcSlot,
    region: Region,
) -> Result<(), Errno> {
    let owner = processes.slot(current).id;
    if !allocator.owns(owner, region)
        || processes
            .slot(current)
            .capabilities
            .find(Object::Memory { owner, region }, cap::CONTROL)
            .is_none()
    {
        return Err(Errno::Eperm);
    }
    if scheduler.tasks.iter().any(|t| {
        t.process == current
            && t.state != State::Terminated
            && region.contains(t.stack.base, t.stack.size)
    }) {
        return Err(Errno::Ebusy);
    }
    invalidate_object(processes, scheduler, Object::Memory { owner, region }, None);
    allocator.free(owner, region).unwrap();
    Ok(())
}

/// Update the base priority of a live thread owned by `current` within its ceiling.
pub fn set_thread_priority<C: Copy>(
    processes: &Processes,
    scheduler: &mut Scheduler<C>,
    current: ProcSlot,
    target_tid: Tid,
    priority: u32,
) -> Result<(), Errno> {
    if priority > u32::from(processes.slot(current).priority_ceiling) {
        return Err(Errno::Einval);
    }
    let thread = scheduler
        .tasks
        .iter()
        .position(|t| t.id == target_tid && t.process == current && t.state != State::Terminated)
        .map(ThreadSlot)
        .ok_or(Errno::Eperm)?;
    scheduler.task_mut(thread).priority = priority as u8;
    scheduler.inherit();
    Ok(())
}

/// Validate entry permissions, owned non-overlapping stack region, priority
/// ceiling, and hart affinity before creating a new thread in `current`.
#[allow(clippy::too_many_arguments)]
pub fn spawn_user_thread<C: Copy>(
    processes: &Processes,
    allocator: &RegionAllocator,
    scheduler: &mut Scheduler<C>,
    current: ProcSlot,
    entry: usize,
    stack: Region,
    priority: u32,
    affinity: u32,
    make_context: impl FnOnce(usize, usize) -> C,
) -> Result<Tid, Errno> {
    let proc = processes.slot(current);
    if entry & 1 != 0
        || !proc.memory.permits(entry, 2, process::EXECUTE)
        || stack.base & 15 != 0
        || stack.size < 256
        || stack.size & 15 != 0
        || !proc.memory.permits(stack.base, stack.size, process::WRITE)
        || priority > u32::from(proc.priority_ceiling)
        || affinity != 1
        || !allocator.owns(proc.id, stack)
    {
        return Err(Errno::Einval);
    }
    let end = stack.base + stack.size;
    if scheduler.tasks.iter().any(|t| {
        t.process == current
            && t.state != State::Terminated
            && stack.base < t.stack.base + t.stack.size
            && t.stack.base < end
    }) {
        return Err(Errno::Einval);
    }
    let context = make_context(entry, end);
    scheduler.create_thread(
        current,
        context,
        stack,
        entry,
        priority as u8,
        affinity as u8,
    )
}

/// Terminate `scheduler.current`. If it was the last live thread of its process,
/// reap all process-owned endpoints, shared-memory grants, capabilities, and
/// physical allocations, returning `Some(ReapedProcess)` for platform cleanup.
pub fn terminate_current_thread<C: Context>(
    processes: &mut Processes,
    ipc: &mut Ipc,
    calls: &mut CallTable,
    allocator: &mut RegionAllocator,
    scheduler: &mut Scheduler<C>,
    mut deliver: impl FnMut(&AddressSpace, usize, usize, &[u8]) -> Result<(), Errno>,
) -> Option<ReapedProcess> {
    let process = scheduler.current().process;
    let exiting = scheduler.current;
    calls.deactivate(exiting);
    calls.fail_for_server(scheduler, exiting, Errno::Epipe);
    scheduler.terminate();
    if scheduler
        .tasks
        .iter()
        .any(|t| t.process == process && t.state != State::Terminated)
    {
        return None;
    }
    let owner = processes.slot(process).id;
    processes.slot_mut(process).state = Lifecycle::Dying;
    let objects: [Object; ipc::MAX_ENDPOINTS] = core::array::from_fn(|i| ipc.object(i));
    calls.fail_where(scheduler, Errno::Epipe, |call, _| {
        ipc.owner(call.endpoint()) == Some(process)
    });
    let closed = ipc.remove_task(process);
    for (i, is_closed) in closed.iter().enumerate() {
        if *is_closed {
            invalidate_object(processes, scheduler, objects[i], None);
        }
    }
    for capability in processes
        .slot(process)
        .capabilities
        .objects()
        .into_iter()
        .flatten()
    {
        if let Object::Memory {
            owner: mem_owner, ..
        } = capability.object
            && mem_owner == owner
        {
            invalidate_object(processes, scheduler, capability.object, Some(process));
        }
    }
    processes.slot_mut(process).capabilities.clear();
    allocator.release_owner(owner);
    processes.slot_mut(process).state = Lifecycle::Free;
    processes.slot_mut(process).memory = AddressSpace::EMPTY;
    let mut first_woken = None;
    for (ep, is_closed) in closed.into_iter().enumerate() {
        if is_closed {
            let woken =
                complete_receivers(processes, ipc, calls, scheduler, ep, true, &mut deliver);
            if first_woken.is_none() {
                first_woken = woken;
            }
        }
    }
    Some(ReapedProcess {
        slot: process,
        pid: owner,
        first_woken,
    })
}

/// Terminate every thread belonging to `scheduler.current`'s process and reap
/// the process immediately.
pub fn terminate_process<C: Context>(
    processes: &mut Processes,
    ipc: &mut Ipc,
    calls: &mut CallTable,
    allocator: &mut RegionAllocator,
    scheduler: &mut Scheduler<C>,
    deliver: impl FnMut(&AddressSpace, usize, usize, &[u8]) -> Result<(), Errno>,
) -> ReapedProcess {
    let current = scheduler.current().process;
    for i in 0..TASKS {
        let slot = ThreadSlot(i);
        if scheduler.task(slot).process == current && slot != scheduler.current {
            calls.deactivate(slot);
            calls.fail_for_server(scheduler, slot, Errno::Epipe);
            let task = scheduler.task_mut(slot);
            task.state = State::Terminated;
            task.wait = Wait::None;
            task.deadline = None;
        }
    }
    terminate_current_thread(processes, ipc, calls, allocator, scheduler, deliver)
        .expect("process has no remaining live threads")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::PAGE;

    #[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
    struct TestCtx {
        a0: u32,
        a1: u32,
    }

    impl Context for TestCtx {
        fn set_return(&mut self, value: u32) {
            self.a0 = value;
        }
        fn set_return2(&mut self, value: u32) {
            self.a1 = value;
        }
    }

    struct Harness {
        processes: Processes,
        ipc: Ipc,
        calls: CallTable,
        allocator: RegionAllocator,
        scheduler: Scheduler<TestCtx>,
    }

    const CODE: Region = Region {
        base: 0x4ff0_0000,
        size: 8192,
    };

    fn harness() -> Harness {
        let stacks = [
            Region {
                base: 0x4ff1_0000,
                size: 4096,
            },
            Region {
                base: 0x4ff1_1000,
                size: 4096,
            },
            Region {
                base: 0x4ff1_2000,
                size: 4096,
            },
        ];
        let mut processes = Processes::new();
        for stack in stacks {
            let mut space = AddressSpace::EMPTY;
            space
                .insert(CODE, process::READ | process::EXECUTE)
                .unwrap();
            space.insert(stack, process::READ | process::WRITE).unwrap();
            processes.create(space).unwrap();
        }
        Harness {
            processes,
            ipc: Ipc::new(),
            calls: CallTable::new(),
            allocator: RegionAllocator::new(0x4ff4_0000, 0x4ff4_0000 + 32 * PAGE),
            scheduler: Scheduler::new([TestCtx::default(); 3], [CODE.base; 3], stacks),
        }
    }

    fn deliver_checked(
        space: &AddressSpace,
        pointer: usize,
        capacity: usize,
        data: &[u8],
    ) -> Result<(), Errno> {
        if data.len() > capacity {
            return Err(Errno::Emsgsize);
        }
        if !space.permits(pointer, capacity, process::WRITE) {
            return Err(Errno::Efault);
        }
        Ok(())
    }

    fn make_msg(payload: &[u8], token: u32) -> Message {
        let mut m = Message::EMPTY;
        m.length = payload.len();
        m.reply_token = token;
        m.data[..payload.len()].copy_from_slice(payload);
        m
    }

    #[test]
    fn create_endpoint_rolls_back_on_cap_exhaustion_and_grant_enforces_one_level() {
        let mut h = harness();
        let p0 = ProcSlot(0);
        let p1 = ProcSlot(1);
        let p2 = ProcSlot(2);

        let ep = create_endpoint(&mut h.ipc, &mut h.processes, p0).unwrap();
        assert_eq!(grant(&mut h.processes, p0, ep, p1, 0), Err(Errno::Eperm));
        assert_eq!(
            grant(&mut h.processes, p0, ep, p1, cap::SEND | cap::GRANT),
            Err(Errno::Eperm)
        );
        assert_eq!(
            grant(&mut h.processes, p0, ep, p1, cap::SEND | cap::CONTROL),
            Err(Errno::Eperm)
        );

        let delegated = grant(&mut h.processes, p0, ep, p1, cap::SEND).unwrap();
        // Recipient cannot re-grant to p2.
        assert_eq!(
            grant(&mut h.processes, p1, delegated, p2, cap::SEND),
            Err(Errno::Eperm)
        );

        // Exhaust p2's capability table and ensure create_endpoint frees the IPC slot.
        for _ in 0..16 {
            grant(&mut h.processes, p0, ep, p2, cap::SEND).unwrap();
        }
        assert_eq!(
            create_endpoint(&mut h.ipc, &mut h.processes, p2),
            Err(Errno::Enospc)
        );
        // Only 1 of MAX_ENDPOINTS is actually occupied after rollback.
        for _ in 1..ipc::MAX_ENDPOINTS {
            create_endpoint(&mut h.ipc, &mut h.processes, p0).unwrap();
        }
    }

    #[test]
    fn rpc_cycle_detection_rejects_self_dead_server_and_three_way_cycle() {
        let mut h = harness();
        let p0 = ProcSlot(0);
        let p1 = ProcSlot(1);
        let p2 = ProcSlot(2);

        let ep0 = create_endpoint(&mut h.ipc, &mut h.processes, p0).unwrap();
        let ep1 = create_endpoint(&mut h.ipc, &mut h.processes, p1).unwrap();
        let ep2 = create_endpoint(&mut h.ipc, &mut h.processes, p2).unwrap();

        let p0_to_p1 = grant(&mut h.processes, p1, ep1, p0, cap::SEND).unwrap();
        let p1_to_p2 = grant(&mut h.processes, p2, ep2, p1, cap::SEND).unwrap();
        let p2_to_p0 = grant(&mut h.processes, p0, ep0, p2, cap::SEND).unwrap();

        // Self-RPC is rejected immediately with EDEADLK.
        assert_eq!(
            find_rpc_server(&h.processes, &h.ipc, &h.scheduler, p0, ThreadSlot(0), ep0),
            Err(Errno::Edeadlk)
        );

        // Thread 0 (p0) calls p1.
        h.scheduler.current = ThreadSlot(0);
        let (idx1, srv1) = find_rpc_server(
            &h.processes,
            &h.ipc,
            &h.scheduler,
            p0,
            ThreadSlot(0),
            p0_to_p1,
        )
        .unwrap();
        let (g0, t0) = h.calls.next_token(ThreadSlot(0)).unwrap();
        commit_ipc_call(
            &h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            idx1,
            srv1,
            g0,
            make_msg(b"req0", t0),
            0x4ff1_0100,
            16,
            None,
            deliver_checked,
        )
        .unwrap();

        // Thread 1 (p1) calls p2 -> allowed (chain p0 -> p1 -> p2).
        h.scheduler.current = ThreadSlot(1);
        h.scheduler.task_mut(ThreadSlot(1)).state = State::Running;
        let (idx2, srv2) = find_rpc_server(
            &h.processes,
            &h.ipc,
            &h.scheduler,
            p1,
            ThreadSlot(1),
            p1_to_p2,
        )
        .unwrap();
        let (g1, t1) = h.calls.next_token(ThreadSlot(1)).unwrap();
        commit_ipc_call(
            &h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            idx2,
            srv2,
            g1,
            make_msg(b"req1", t1),
            0x4ff1_1100,
            16,
            None,
            deliver_checked,
        )
        .unwrap();

        // Thread 2 (p2) attempts to call p0 -> transitive cycle p0 -> p1 -> p2 -> p0!
        assert_eq!(
            find_rpc_server(
                &h.processes,
                &h.ipc,
                &h.scheduler,
                p2,
                ThreadSlot(2),
                p2_to_p0
            ),
            Err(Errno::Edeadlk)
        );
    }

    #[test]
    fn rpc_call_receiver_wakeup_priority_inheritance_and_reply_round_trip() {
        let mut h = harness();
        let client_proc = ProcSlot(2);
        let server_proc = ProcSlot(0);
        // Server starts low priority (0), client starts high priority (3).
        h.scheduler.task_mut(ThreadSlot(0)).priority = 0;
        h.scheduler.task_mut(ThreadSlot(0)).effective_priority = 0;
        h.scheduler.task_mut(ThreadSlot(2)).priority = 3;
        h.scheduler.task_mut(ThreadSlot(2)).effective_priority = 3;

        let ep_handle = create_endpoint(&mut h.ipc, &mut h.processes, server_proc).unwrap();
        let send_handle = grant(
            &mut h.processes,
            server_proc,
            ep_handle,
            client_proc,
            cap::SEND,
        )
        .unwrap();
        let ep_idx =
            resolve_endpoint(&h.processes, &h.ipc, server_proc, ep_handle, cap::RECEIVE).unwrap();

        // Server blocks waiting to receive into its stack buffer.
        h.scheduler.current = ThreadSlot(0);
        h.scheduler.block(Wait::Receive {
            endpoint: ep_idx,
            handle: ep_handle,
            pointer: 0x4ff1_0200,
            capacity: 32,
        });

        // Client performs RPC call -> wakes server, binds reply token, donates priority 3.
        h.scheduler.current = ThreadSlot(2);
        h.scheduler.task_mut(ThreadSlot(2)).state = State::Running;
        let (ep, srv) = find_rpc_server(
            &h.processes,
            &h.ipc,
            &h.scheduler,
            client_proc,
            ThreadSlot(2),
            send_handle,
        )
        .unwrap();
        let (generation, token) = h.calls.next_token(ThreadSlot(2)).unwrap();
        let woken = commit_ipc_call(
            &h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            ep,
            srv,
            generation,
            make_msg(b"ping", token),
            0x4ff1_2200,
            16,
            None,
            deliver_checked,
        )
        .unwrap();
        assert_eq!(woken, Some(Tid(1)));
        assert_eq!(h.scheduler.task(ThreadSlot(0)).state, State::Ready);
        assert_eq!(h.scheduler.task(ThreadSlot(0)).context.a0, 4);
        assert_eq!(h.scheduler.task(ThreadSlot(0)).context.a1, token);
        assert_eq!(h.scheduler.task(ThreadSlot(0)).effective_priority, 3);

        // Wrong process cannot spoof the reply.
        assert_eq!(
            prepare_ipc_reply(&h.calls, &h.scheduler, ProcSlot(1), token, 4),
            Err(Errno::Eperm)
        );
        // Oversized reply rejected without consuming the active call.
        assert_eq!(
            prepare_ipc_reply(&h.calls, &h.scheduler, server_proc, token, 17),
            Err(Errno::Emsgsize)
        );

        let pending = prepare_ipc_reply(&h.calls, &h.scheduler, server_proc, token, 4).unwrap();
        assert_eq!(pending.client, ThreadSlot(2));
        assert_eq!(pending.target, client_proc);
        finish_ipc_reply(&mut h.calls, &mut h.scheduler, pending.client, 4);

        assert_eq!(h.scheduler.task(ThreadSlot(2)).state, State::Ready);
        assert_eq!(h.scheduler.task(ThreadSlot(2)).context.a0, 4);
        // Priority boost drops once reply completes.
        assert_eq!(h.scheduler.task(ThreadSlot(0)).effective_priority, 0);
        // Replaying the consumed token fails with EPIPE.
        assert_eq!(
            prepare_ipc_reply(&h.calls, &h.scheduler, server_proc, token, 4),
            Err(Errno::Epipe)
        );
    }

    #[test]
    fn memory_alloc_grant_revoke_unmaps_peer_and_wakes_blocked_receiver_with_efault() {
        let mut h = harness();
        let owner = ProcSlot(0);
        let peer = ProcSlot(1);
        let peer_pid = h.processes.slot(peer).id;

        let region = alloc_memory(&mut h.processes, &mut h.allocator, owner, PAGE).unwrap();
        let cap_handle = memory_capability(&h.processes, owner, region.base).unwrap();

        // Write-only grant without READ is rejected; non-memory with require_memory is rejected.
        assert_eq!(
            grant_or_map(
                &mut h.processes,
                owner,
                cap_handle,
                peer_pid,
                cap::WRITE,
                true
            ),
            Err(Errno::Einval)
        );
        let ep = create_endpoint(&mut h.ipc, &mut h.processes, peer).unwrap();
        assert_eq!(
            grant_or_map(&mut h.processes, peer, ep, Pid(1), cap::SEND, true),
            Err(Errno::Einval)
        );

        let peer_mem = grant_or_map(
            &mut h.processes,
            owner,
            cap_handle,
            peer_pid,
            cap::READ | cap::WRITE,
            true,
        )
        .unwrap();
        assert!(h.processes.slot(peer).memory.permits(
            region.base,
            64,
            process::READ | process::WRITE
        ));

        // Peer blocks in Wait::Receive using the shared memory buffer.
        let ep_idx = resolve_endpoint(&h.processes, &h.ipc, peer, ep, cap::RECEIVE).unwrap();
        h.scheduler.current = ThreadSlot(1);
        h.scheduler.block(Wait::Receive {
            endpoint: ep_idx,
            handle: ep,
            pointer: region.base + 32,
            capacity: 64,
        });

        // Owner revokes capability -> peer loses mapping + handle, blocked recv wakes with EFAULT.
        revoke_capability(
            &mut h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            owner,
            cap_handle,
            deliver_checked,
        )
        .unwrap();

        assert!(
            !h.processes
                .slot(peer)
                .memory
                .permits(region.base, 1, process::READ)
        );
        assert_eq!(
            h.processes.slot(peer).capabilities.get(peer_mem, 0).err(),
            Some(Errno::Ebadf)
        );
        assert_eq!(h.scheduler.task(ThreadSlot(1)).state, State::Ready);
        assert_eq!(
            h.scheduler.task(ThreadSlot(1)).context.a0,
            Errno::Efault.to_abi()
        );
        // Owner still retains its own mapping and capability.
        assert!(
            h.processes
                .slot(owner)
                .memory
                .permits(region.base, PAGE, process::WRITE)
        );
        assert!(memory_capability(&h.processes, owner, region.base).is_ok());
    }

    #[test]
    fn free_memory_rejects_active_thread_stack_and_cleans_up_mappings_and_waiters() {
        let mut h = harness();
        let p0 = ProcSlot(0);
        let stack = alloc_memory(&mut h.processes, &mut h.allocator, p0, PAGE).unwrap();

        // Overlapping existing bootstrap stack or unaligned entry is rejected.
        let boot_stack = h.scheduler.task(ThreadSlot(0)).stack;
        assert_eq!(
            spawn_user_thread(
                &h.processes,
                &h.allocator,
                &mut h.scheduler,
                p0,
                CODE.base,
                boot_stack,
                1,
                1,
                |_, _| TestCtx::default(),
            ),
            Err(Errno::Einval)
        );

        let tid = spawn_user_thread(
            &h.processes,
            &h.allocator,
            &mut h.scheduler,
            p0,
            CODE.base,
            stack,
            1,
            1,
            |_, _| TestCtx::default(),
        )
        .unwrap();
        assert_eq!(tid, Tid(4));

        // Cannot free a region backing a live thread stack.
        assert_eq!(
            free_memory(
                &mut h.processes,
                &mut h.allocator,
                &mut h.scheduler,
                p0,
                stack
            ),
            Err(Errno::Ebusy)
        );

        // Terminate only the worker thread (slot 3); process p0 stays alive because slot 0 lives.
        h.scheduler.current = ThreadSlot(3);
        assert!(
            terminate_current_thread(
                &mut h.processes,
                &mut h.ipc,
                &mut h.calls,
                &mut h.allocator,
                &mut h.scheduler,
                deliver_checked,
            )
            .is_none()
        );
        assert_eq!(h.processes.slot(p0).state, Lifecycle::Alive);

        // Now freeing the worker stack succeeds.
        free_memory(
            &mut h.processes,
            &mut h.allocator,
            &mut h.scheduler,
            p0,
            stack,
        )
        .unwrap();
        assert!(!h.allocator.owns(h.processes.slot(p0).id, stack));
    }

    #[test]
    fn endpoint_close_peer_vs_owner_and_last_thread_exit_reap_all_resources() {
        let mut h = harness();
        let server = ProcSlot(0);
        let client_a = ProcSlot(1);
        let client_b = ProcSlot(2);

        let ep = create_endpoint(&mut h.ipc, &mut h.processes, server).unwrap();
        let send_a = grant(&mut h.processes, server, ep, client_a, cap::SEND).unwrap();
        let send_b = grant(&mut h.processes, server, ep, client_b, cap::SEND).unwrap();
        let shared = alloc_memory(&mut h.processes, &mut h.allocator, server, PAGE).unwrap();
        let shared_cap = memory_capability(&h.processes, server, shared.base).unwrap();
        let client_a_pid = h.processes.slot(client_a).id;
        grant_or_map(
            &mut h.processes,
            server,
            shared_cap,
            client_a_pid,
            cap::READ,
            true,
        )
        .unwrap();

        // Client A closes its delegated handle -> endpoint stays alive for Client B and Server.
        close_endpoint(
            &mut h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            client_a,
            send_a,
            deliver_checked,
        )
        .unwrap();
        assert_eq!(
            resolve_endpoint(&h.processes, &h.ipc, client_a, send_a, cap::SEND),
            Err(Errno::Ebadf)
        );
        assert!(resolve_endpoint(&h.processes, &h.ipc, client_b, send_b, cap::SEND).is_ok());

        // Client B starts an RPC call to Server.
        h.scheduler.current = ThreadSlot(2);
        h.scheduler.task_mut(ThreadSlot(2)).state = State::Running;
        let (ep_idx, srv_slot) = find_rpc_server(
            &h.processes,
            &h.ipc,
            &h.scheduler,
            client_b,
            ThreadSlot(2),
            send_b,
        )
        .unwrap();
        let (gen_b, tok_b) = h.calls.next_token(ThreadSlot(2)).unwrap();
        commit_ipc_call(
            &h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            ep_idx,
            srv_slot,
            gen_b,
            make_msg(b"call", tok_b),
            0x4ff1_2100,
            16,
            None,
            deliver_checked,
        )
        .unwrap();

        // Server exits its sole thread -> reaps endpoints, shared memory, capabilities, and wakes Client B with EPIPE.
        h.scheduler.current = ThreadSlot(0);
        let reaped = terminate_current_thread(
            &mut h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.allocator,
            &mut h.scheduler,
            deliver_checked,
        )
        .unwrap();
        assert_eq!(reaped.slot, server);
        assert_eq!(reaped.pid, Pid(1));
        assert_eq!(h.processes.slot(server).state, Lifecycle::Free);
        assert!(!h.allocator.owns(Pid(1), shared));
        assert!(
            !h.processes
                .slot(client_a)
                .memory
                .permits(shared.base, 1, process::READ)
        );
        assert_eq!(
            resolve_endpoint(&h.processes, &h.ipc, client_b, send_b, cap::SEND),
            Err(Errno::Ebadf)
        );
        assert_eq!(h.scheduler.task(ThreadSlot(2)).state, State::Ready);
        assert_eq!(
            h.scheduler.task(ThreadSlot(2)).context.a0,
            Errno::Epipe.to_abi()
        );
    }

    #[test]
    fn authority_capabilities_enforce_rights_delegation_and_revocation() {
        let mut h = harness();
        let shell = ProcSlot(1);
        let peer = ProcSlot(2);
        let peer_pid = h.processes.slot(peer).id;

        let sentinel_root = h
            .processes
            .slot_mut(shell)
            .capabilities
            .install(Capability {
                object: Object::Authority(Authority::Sentinel),
                rights: cap::READ | cap::WRITE | cap::WAIT | cap::CONTROL | cap::GRANT,
            })
            .unwrap();
        let ep = create_endpoint(&mut h.ipc, &mut h.processes, shell).unwrap();

        // Unprivileged process and wrong object type are rejected.
        assert_eq!(
            require_authority(
                &h.processes,
                ProcSlot(0),
                sentinel_root,
                Authority::Sentinel,
                cap::READ,
            ),
            Err(Errno::Ebadf)
        );
        assert_eq!(
            require_authority(&h.processes, shell, ep, Authority::Sentinel, cap::READ),
            Err(Errno::Ebadf)
        );
        assert_eq!(
            require_authority(
                &h.processes,
                shell,
                sentinel_root,
                Authority::Display,
                cap::CONTROL,
            ),
            Err(Errno::Ebadf)
        );

        // Delegate read-only Sentinel authority to peer; CONTROL/WRITE remain denied.
        let peer_ro = grant_or_map(
            &mut h.processes,
            shell,
            sentinel_root,
            peer_pid,
            cap::READ,
            false,
        )
        .unwrap();
        assert_eq!(
            require_authority(&h.processes, peer, peer_ro, Authority::Sentinel, cap::READ),
            Ok(())
        );
        assert_eq!(
            require_authority(
                &h.processes,
                peer,
                peer_ro,
                Authority::Sentinel,
                cap::CONTROL,
            ),
            Err(Errno::Eperm)
        );

        // Revoking root invalidates peer handle while preserving shell root authority.
        revoke_capability(
            &mut h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            shell,
            sentinel_root,
            deliver_checked,
        )
        .unwrap();
        assert_eq!(
            require_authority(&h.processes, peer, peer_ro, Authority::Sentinel, cap::READ),
            Err(Errno::Ebadf)
        );
        assert_eq!(
            require_authority(
                &h.processes,
                shell,
                sentinel_root,
                Authority::Sentinel,
                cap::CONTROL,
            ),
            Ok(())
        );
    }

    #[test]
    fn terminate_process_deactivates_sibling_client_calls_and_reaps_immediately() {
        let mut h = harness();
        let p0 = ProcSlot(0);
        let p1 = ProcSlot(1);

        let ep1 = create_endpoint(&mut h.ipc, &mut h.processes, p1).unwrap();
        let send_to_p1 = grant(&mut h.processes, p1, ep1, p0, cap::SEND).unwrap();
        let stack = alloc_memory(&mut h.processes, &mut h.allocator, p0, PAGE).unwrap();
        spawn_user_thread(
            &h.processes,
            &h.allocator,
            &mut h.scheduler,
            p0,
            CODE.base,
            stack,
            1,
            1,
            |_, _| TestCtx::default(),
        )
        .unwrap();

        // Sibling worker thread (slot 3) starts an RPC call to p1, and p1 receives it (binding reply).
        h.scheduler.current = ThreadSlot(3);
        h.scheduler.task_mut(ThreadSlot(3)).state = State::Running;
        let (ep_idx, srv_slot) = find_rpc_server(
            &h.processes,
            &h.ipc,
            &h.scheduler,
            p0,
            ThreadSlot(3),
            send_to_p1,
        )
        .unwrap();
        let (gen_w, tok_w) = h.calls.next_token(ThreadSlot(3)).unwrap();
        commit_ipc_call(
            &h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.scheduler,
            ep_idx,
            srv_slot,
            gen_w,
            make_msg(b"work", tok_w),
            stack.base + 64,
            16,
            None,
            deliver_checked,
        )
        .unwrap();
        let msg = h.ipc.receive(ep_idx, 32).unwrap().unwrap();
        h.calls
            .bind_reply(&mut h.scheduler, msg.reply_token, ThreadSlot(1));

        // Thread 0 invokes PROCESS_EXIT -> kills sibling slot 3, deactivates tok_w, and reaps p0.
        h.scheduler.current = ThreadSlot(0);
        let reaped = terminate_process(
            &mut h.processes,
            &mut h.ipc,
            &mut h.calls,
            &mut h.allocator,
            &mut h.scheduler,
            deliver_checked,
        );
        assert_eq!(reaped.slot, p0);
        assert_eq!(h.scheduler.task(ThreadSlot(3)).state, State::Terminated);
        assert_eq!(h.calls.lookup_for_reply(tok_w), Err(Errno::Epipe));
        assert_eq!(h.processes.slot(p0).state, Lifecycle::Free);
    }
}
