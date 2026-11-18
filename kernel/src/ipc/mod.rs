//! Single-hart IPC. Serialize with scheduler under masked machine IRQs.
//! No user pointers inside this module.
use crate::sched::TASKS;
pub const MAX_ENDPOINTS: usize = 8;
pub const HANDLES: usize = 6;
pub const MESSAGE_BYTES: usize = 96;
pub const QUEUE_DEPTH: usize = 4;
pub const SEND: u8 = 1;
pub const RECEIVE: u8 = 2;
pub const EBADF: u32 = (-9i32) as u32;
pub const EPERM: u32 = (-1i32) as u32;
pub const EAGAIN: u32 = (-11i32) as u32;
pub const ENOSPC: u32 = (-28i32) as u32;
pub const EPIPE: u32 = (-32i32) as u32;
pub const EMSGSIZE: u32 = (-90i32) as u32;
#[derive(Clone, Copy)]
pub struct Message {
    pub data: [u8; MESSAGE_BYTES],
    pub length: usize,
}
impl Message {
    pub const EMPTY: Self = Self {
        data: [0; MESSAGE_BYTES],
        length: 0,
    };
}
#[derive(Clone, Copy)]
struct Endpoint {
    owner: Option<usize>,
    queue: [Message; QUEUE_DEPTH],
    head: usize,
    count: usize,
}
impl Endpoint {
    const EMPTY: Self = Self {
        owner: None,
        queue: [Message::EMPTY; QUEUE_DEPTH],
        head: 0,
        count: 0,
    };
}
#[derive(Clone, Copy)]
struct Handle {
    endpoint: Option<usize>,
    rights: u8,
    generation: u32,
}
impl Handle {
    const EMPTY: Self = Self {
        endpoint: None,
        rights: 0,
        generation: 0,
    };
}
pub struct Ipc {
    endpoints: [Endpoint; MAX_ENDPOINTS],
    handles: [[Handle; HANDLES]; TASKS],
}
impl Default for Ipc {
    fn default() -> Self {
        Self::new()
    }
}
impl Ipc {
    pub const fn new() -> Self {
        Self {
            endpoints: [Endpoint::EMPTY; MAX_ENDPOINTS],
            handles: [[Handle::EMPTY; HANDLES]; TASKS],
        }
    }
    fn install(&mut self, task: usize, endpoint: usize, rights: u8) -> Result<u32, u32> {
        let (slot, h) = self.handles[task]
            .iter_mut()
            .enumerate()
            .find(|(_, h)| h.endpoint.is_none() && h.generation < 0x7f_ffff)
            .ok_or(ENOSPC)?;
        // Retire generations instead of wrapping: stale handles cannot revive.
        h.generation += 1;
        h.endpoint = Some(endpoint);
        h.rights = rights;
        Ok((h.generation << 8) | (slot as u32 + 1))
    }
    fn slot(&self, task: usize, handle: u32) -> Result<usize, u32> {
        let slot = (handle & 255).wrapping_sub(1) as usize;
        let h = self.handles[task].get(slot).ok_or(EBADF)?;
        if h.endpoint.is_none() || h.generation != handle >> 8 {
            return Err(EBADF);
        }
        Ok(slot)
    }
    pub fn resolve(&self, task: usize, handle: u32, rights: u8) -> Result<usize, u32> {
        let h = &self.handles[task][self.slot(task, handle)?];
        if h.rights & rights != rights {
            return Err(EPERM);
        }
        let ep = h.endpoint.ok_or(EBADF)?;
        if self.endpoints[ep].owner.is_none() {
            return Err(EPIPE);
        }
        Ok(ep)
    }
    pub fn create(&mut self, owner: usize) -> Result<u32, u32> {
        let ep = self
            .endpoints
            .iter()
            .position(|e| e.owner.is_none())
            .ok_or(ENOSPC)?;
        let handle = self.install(owner, ep, SEND | RECEIVE)?;
        self.endpoints[ep] = Endpoint {
            owner: Some(owner),
            ..Endpoint::EMPTY
        };
        Ok(handle)
    }
    /// Boot-only explicit assignment, not exposed as a syscall.
    pub fn grant_send(&mut self, owner: usize, handle: u32, peer: usize) -> Result<u32, u32> {
        let ep = self.resolve(owner, handle, RECEIVE)?;
        if self.endpoints[ep].owner != Some(owner) {
            return Err(EPERM);
        }
        self.install(peer, ep, SEND)
    }
    /// Full queue => EAGAIN, no partial delivery and no sender blocking.
    pub fn send(&mut self, task: usize, handle: u32, message: Message) -> Result<usize, u32> {
        if message.length > MESSAGE_BYTES {
            return Err(EMSGSIZE);
        }
        let ep = self.resolve(task, handle, SEND)?;
        let endpoint = &mut self.endpoints[ep];
        if endpoint.count == QUEUE_DEPTH {
            return Err(EAGAIN);
        }
        endpoint.queue[(endpoint.head + endpoint.count) % QUEUE_DEPTH] = message;
        endpoint.count += 1;
        Ok(ep)
    }
    /// EMSGSIZE leaves message queued. Receive rights are owner-only.
    pub fn receive(&mut self, ep: usize, capacity: usize) -> Result<Option<Message>, u32> {
        let endpoint = &mut self.endpoints[ep];
        if endpoint.owner.is_none() {
            return Err(EPIPE);
        }
        if endpoint.count == 0 {
            return Ok(None);
        }
        if endpoint.queue[endpoint.head].length > capacity {
            return Err(EMSGSIZE);
        }
        let result = endpoint.queue[endpoint.head];
        endpoint.head = (endpoint.head + 1) % QUEUE_DEPTH;
        endpoint.count -= 1;
        Ok(Some(result))
    }
    /// Owner closes endpoint and all aliases; peer only drops its SEND alias.
    pub fn close(&mut self, task: usize, handle: u32) -> Result<Option<usize>, u32> {
        let slot = self.slot(task, handle)?;
        let ep = self.handles[task][slot].endpoint.ok_or(EBADF)?;
        if self.endpoints[ep].owner == Some(task) {
            self.destroy(ep);
            Ok(Some(ep))
        } else {
            self.handles[task][slot].endpoint = None;
            Ok(None)
        }
    }
    fn destroy(&mut self, ep: usize) {
        self.endpoints[ep] = Endpoint::EMPTY;
        for table in &mut self.handles {
            for h in table {
                if h.endpoint == Some(ep) {
                    h.endpoint = None;
                }
            }
        }
    }
    pub fn remove_task(&mut self, task: usize) -> [bool; MAX_ENDPOINTS] {
        let mut closed = [false; MAX_ENDPOINTS];
        for (ep, flag) in closed.iter_mut().enumerate() {
            if self.endpoints[ep].owner == Some(task) {
                self.destroy(ep);
                *flag = true;
            }
        }
        for h in &mut self.handles[task] {
            h.endpoint = None;
        }
        closed
    }
}
