//! Single-hart IPC. Serialize with scheduler under masked machine IRQs.
//! No user pointers inside this module.
use crate::{capability::Object, errno::Errno, id::ProcSlot};
pub const MAX_ENDPOINTS: usize = 8;
pub const HANDLES: usize = 6;
pub const MESSAGE_BYTES: usize = 96;
pub const QUEUE_DEPTH: usize = 4;
pub const SEND: u16 = crate::capability::SEND;
pub const RECEIVE: u16 = crate::capability::RECEIVE;
#[derive(Clone, Copy)]
pub struct Message {
    pub data: [u8; MESSAGE_BYTES],
    pub length: usize,
    pub reply_token: u32,
}
impl Message {
    pub const EMPTY: Self = Self {
        data: [0; MESSAGE_BYTES],
        length: 0,
        reply_token: 0,
    };
}
#[derive(Clone, Copy)]
struct Endpoint {
    owner: Option<ProcSlot>,
    generation: u32,
    queue: [Message; QUEUE_DEPTH],
    head: usize,
    count: usize,
}
impl Endpoint {
    const EMPTY: Self = Self {
        owner: None,
        generation: 0,
        queue: [Message::EMPTY; QUEUE_DEPTH],
        head: 0,
        count: 0,
    };
}
pub struct Ipc {
    endpoints: [Endpoint; MAX_ENDPOINTS],
    generation: u32,
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
            generation: 0,
        }
    }
    pub fn resolve(&self, object: Object) -> Result<usize, Errno> {
        let Object::Endpoint { index, generation } = object else {
            return Err(Errno::Ebadf);
        };
        let ep = self.endpoints.get(index).ok_or(Errno::Ebadf)?;
        if ep.owner.is_none() || ep.generation != generation {
            return Err(Errno::Epipe);
        }
        Ok(index)
    }
    pub fn create(&mut self, owner: ProcSlot) -> Result<Object, Errno> {
        let index = self
            .endpoints
            .iter()
            .position(|e| e.owner.is_none())
            .ok_or(Errno::Enospc)?;
        self.generation = self.generation.checked_add(1).ok_or(Errno::Enospc)?;
        self.endpoints[index] = Endpoint {
            owner: Some(owner),
            generation: self.generation,
            ..Endpoint::EMPTY
        };
        Ok(Object::Endpoint {
            index,
            generation: self.generation,
        })
    }
    pub fn owner(&self, index: usize) -> Option<ProcSlot> {
        self.endpoints.get(index)?.owner
    }
    pub fn is_empty(&self, ep: usize) -> bool {
        self.endpoints[ep].count == 0
    }
    pub fn object(&self, index: usize) -> Object {
        Object::Endpoint {
            index,
            generation: self.endpoints[index].generation,
        }
    }
    /// Full queue => EAGAIN, no partial delivery and no sender blocking.
    pub fn send(&mut self, ep: usize, message: Message) -> Result<usize, Errno> {
        if message.length > MESSAGE_BYTES {
            return Err(Errno::Emsgsize);
        }

        let endpoint = &mut self.endpoints[ep];
        if endpoint.count == QUEUE_DEPTH {
            return Err(Errno::Eagain);
        }
        endpoint.queue[(endpoint.head + endpoint.count) % QUEUE_DEPTH] = message;
        endpoint.count += 1;
        Ok(ep)
    }
    /// EMSGSIZE leaves message queued. Receive rights are owner-only.
    pub fn receive(&mut self, ep: usize, capacity: usize) -> Result<Option<Message>, Errno> {
        let endpoint = &mut self.endpoints[ep];
        if endpoint.owner.is_none() {
            return Err(Errno::Epipe);
        }
        if endpoint.count == 0 {
            return Ok(None);
        }
        if endpoint.queue[endpoint.head].length > capacity {
            return Err(Errno::Emsgsize);
        }
        let result = endpoint.queue[endpoint.head];
        endpoint.head = (endpoint.head + 1) % QUEUE_DEPTH;
        endpoint.count -= 1;
        Ok(Some(result))
    }
    pub fn close(&mut self, ep: usize) {
        self.endpoints[ep] = Endpoint::EMPTY;
    }
    pub fn remove_task(&mut self, task: ProcSlot) -> [bool; MAX_ENDPOINTS] {
        let mut closed = [false; MAX_ENDPOINTS];
        for (ep, flag) in closed.iter_mut().enumerate() {
            if self.endpoints[ep].owner == Some(task) {
                self.close(ep);
                *flag = true;
            }
        }
        closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{id::Pid, task::Region};

    fn make_message(bytes: &[u8], token: u32) -> Message {
        let mut msg = Message::EMPTY;
        msg.length = bytes.len();
        msg.reply_token = token;
        msg.data[..bytes.len()].copy_from_slice(bytes);
        msg
    }

    #[test]
    fn fifo_delivery_backpressure_and_non_destructive_emsgsize() {
        let mut ipc = Ipc::new();
        let obj = ipc.create(ProcSlot(1)).unwrap();
        let ep = ipc.resolve(obj).unwrap();

        assert!(ipc.receive(ep, MESSAGE_BYTES).unwrap().is_none());
        for i in 0..QUEUE_DEPTH {
            ipc.send(ep, make_message(&[b'a' + i as u8; 8], i as u32 + 1))
                .unwrap();
        }
        assert_eq!(ipc.send(ep, make_message(b"extra", 99)), Err(Errno::Eagain));

        // Too-small buffer returns EMSGSIZE without dropping the head message.
        assert_eq!(ipc.receive(ep, 4).err(), Some(Errno::Emsgsize));
        let first = ipc.receive(ep, 8).unwrap().unwrap();
        assert_eq!(&first.data[..first.length], &[b'a'; 8]);
        assert_eq!(first.reply_token, 1);

        // Ring buffer wraps cleanly after dequeuing one item.
        ipc.send(ep, make_message(b"wrap", 42)).unwrap();
        for expected in [b'b', b'c', b'd'] {
            let msg = ipc.receive(ep, MESSAGE_BYTES).unwrap().unwrap();
            assert_eq!(&msg.data[..msg.length], &[expected; 8]);
        }
        let last = ipc.receive(ep, MESSAGE_BYTES).unwrap().unwrap();
        assert_eq!(&last.data[..last.length], b"wrap");
        assert_eq!(last.reply_token, 42);
    }

    #[test]
    fn resolve_and_remove_task_enforce_generations_and_ownership() {
        let mut ipc = Ipc::new();
        assert_eq!(
            ipc.resolve(Object::Memory {
                owner: Pid(1),
                region: Region::default(),
            }),
            Err(Errno::Ebadf)
        );
        let e0 = ipc.create(ProcSlot(1)).unwrap();
        let e1 = ipc.create(ProcSlot(2)).unwrap();
        let idx0 = ipc.resolve(e0).unwrap();
        let idx1 = ipc.resolve(e1).unwrap();

        let closed = ipc.remove_task(ProcSlot(1));
        assert!(closed[idx0]);
        assert!(!closed[idx1]);
        assert_eq!(ipc.resolve(e0), Err(Errno::Epipe));
        assert_eq!(ipc.receive(idx0, MESSAGE_BYTES).err(), Some(Errno::Epipe));
        assert_eq!(ipc.resolve(e1), Ok(idx1));

        // Slot reuse gets a new generation so stale objects stay invalid.
        let e0_new = ipc.create(ProcSlot(3)).unwrap();
        assert_eq!(ipc.resolve(e0_new), Ok(idx0));
        assert_eq!(ipc.resolve(e0), Err(Errno::Epipe));
    }

    #[test]
    fn endpoint_table_exhaustion_returns_enospc() {
        let mut ipc = Ipc::new();
        for _ in 0..MAX_ENDPOINTS {
            ipc.create(ProcSlot(1)).unwrap();
        }
        assert_eq!(ipc.create(ProcSlot(1)), Err(Errno::Enospc));
    }
}
