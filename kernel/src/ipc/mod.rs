//! Single-hart IPC. Serialize with scheduler under masked machine IRQs.
//! No user pointers inside this module.
use crate::capability::Object;
pub const MAX_ENDPOINTS: usize = 8;
pub const HANDLES: usize = 6;
pub const MESSAGE_BYTES: usize = 96;
pub const QUEUE_DEPTH: usize = 4;
pub const SEND: u16 = crate::capability::SEND;
pub const RECEIVE: u16 = crate::capability::RECEIVE;
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
    owner: Option<usize>,
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
    pub fn resolve(&self, object: Object) -> Result<usize, u32> {
        let Object::Endpoint { index, generation } = object else {
            return Err(EBADF);
        };
        let ep = self.endpoints.get(index).ok_or(EBADF)?;
        if ep.owner.is_none() || ep.generation != generation {
            return Err(EPIPE);
        }
        Ok(index)
    }
    pub fn create(&mut self, owner: usize) -> Result<Object, u32> {
        let index = self
            .endpoints
            .iter()
            .position(|e| e.owner.is_none())
            .ok_or(ENOSPC)?;
        self.generation = self.generation.checked_add(1).ok_or(ENOSPC)?;
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
    pub fn owner(&self, index: usize) -> Option<usize> {
        self.endpoints.get(index)?.owner
    }
    pub fn object(&self, index: usize) -> Object {
        Object::Endpoint {
            index,
            generation: self.endpoints[index].generation,
        }
    }
    /// Full queue => EAGAIN, no partial delivery and no sender blocking.
    pub fn send(&mut self, ep: usize, message: Message) -> Result<usize, u32> {
        if message.length > MESSAGE_BYTES {
            return Err(EMSGSIZE);
        }

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
    pub fn close(&mut self, ep: usize) {
        self.endpoints[ep] = Endpoint::EMPTY;
    }
    pub fn remove_task(&mut self, task: usize) -> [bool; MAX_ENDPOINTS] {
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
