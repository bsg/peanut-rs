use std::{cell::RefCell, rc::Rc};

use peanut::Server;
use wire::{Deserialize, Serialize};

const BITMAP_LEN: usize = 1000 * 1000;

struct Bitmap {
    data: [u8; BITMAP_LEN / 8],
}

impl Bitmap {
    pub fn new() -> Self {
        Bitmap {
            data: [0; BITMAP_LEN / 8],
        }
    }

    pub fn set_bit(&mut self, idx: usize, status: bool) {
        if idx >= BITMAP_LEN {
            panic!("bitmap idx out of range {idx}")
        }

        let byte = unsafe { self.data.get_unchecked_mut(idx / 8) };
        if status {
            *byte |= 1 << idx.wrapping_rem(8);
        } else {
            *byte &= !(1 << idx.wrapping_rem(8));
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.data.as_slice()
    }
}

#[derive(Default, Deserialize)]
#[repr(u32)]
enum In {
    #[default]
    Noop = 0x0,
    Update(u32) = 0x1,
}

#[derive(Default, Serialize)]
#[repr(u32)]
enum Out {
    #[default]
    Noop = 0x0,
    Update(u32) = 0x1,
}

fn main() {
    let bitmap = Rc::new(RefCell::new(Bitmap::new()));
    let mut server = Server::<In, Out>::default();

    server.resource("/", "examples/dotmatrix.html");
    server.resource("/app.js", "examples/dotmatrix.js");

    server.on_connect(|_server, client| {
        let bitmap = bitmap.borrow();
        let bitmap = bitmap.as_slice();
        client.write_blocking(bitmap)?;

        Ok(())
    });

    server.on_message(|server, _client, msg| {
        match msg {
            In::Noop => unreachable!(),
            In::Update(payload) => {
                let status = payload >> 31 & 1;
                let idx = payload & 0x7FFFFFFF;

                bitmap.borrow_mut().set_bit(idx as usize, status == 1);

                for client in server.clients_iter_mut() {
                    client.send(&Out::Update(payload))?;
                }
            }
        }

        Ok(())
    });

    server.run();
}
