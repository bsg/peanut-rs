#![feature(trait_alias)]

use std::{
    cell::UnsafeCell,
    collections::HashMap,
    fs::File,
    io::{Error, ErrorKind, Read, Write},
    marker::PhantomData,
    net::SocketAddr,
    ptr::NonNull,
    time::{Duration, Instant},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use mio::{
    Events, Interest, Poll, Token,
    net::{TcpListener, TcpStream},
};
use ringbuf::RingBuf;
use sha1::{Digest as _, Sha1};
pub use wire::{Deserialize, Serialize};

// TODO
const MAX_CLIENTS: usize = 1024;

trait WriteBlocking {
    fn write_blocking(&mut self, data: &[u8]) -> std::result::Result<usize, std::io::Error>;
}

impl WriteBlocking for TcpStream {
    fn write_blocking(&mut self, data: &[u8]) -> std::result::Result<usize, std::io::Error> {
        let mut written = 0;
        loop {
            match self.write(&data[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => {}
                e @ Err(_) => return e,
            }
        }

        Ok(written)
    }
}

enum Expect {
    Frame,
    PayloadLen16,
    PayloadLen64,
    MaskingKey,
    Payload,
}

// FIXME what are these names lmao
enum ClientState {
    PreHandshake,
    PostHandshake,
}

pub struct Client {
    id: usize,
    state: ClientState,
    expect: Expect,
    stream: TcpStream,
    rx: RingBuf<u8, 1024>,
    tx: RingBuf<u8, 1024>,
    payload_len: usize,
    masking_key: [u8; 4],
}

impl Client {
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn send<T: Serialize>(&mut self, payload: &T) -> std::result::Result<(), std::io::Error> {
        let size = payload.wire_size();

        if size < 126 {
            self.tx.write_all(&[0x82, size as u8])?;
        } else if size <= u16::MAX as usize {
            self.tx.write_all(&[0x82, 126])?;
            self.tx.write_all(&(size as u16).to_le_bytes())?;
        } else {
            self.tx.write_all(&[0x82, 127])?;
            self.tx.write_all(&(size as u64).to_le_bytes())?;
        }

        payload.serialize(&mut self.tx)?;

        self.flush();
        Ok(())
    }

    pub fn write_blocking(&mut self, data: &[u8]) -> std::result::Result<(), std::io::Error> {
        let len = data.len();

        if len < 128 {
            self.stream.write_blocking(&[0x82, len as u8])?;
        } else if len <= u16::MAX as usize {
            self.stream.write_blocking(&[0x82, 126])?;
            self.stream.write_blocking(&(len as u16).to_be_bytes())?;
        } else {
            self.stream.write_blocking(&[0x82, 127])?;
            self.stream.write_blocking(&(len as u64).to_be_bytes())?;
        }
        self.stream.write_blocking(data)?;

        Ok(())
    }

    pub fn flush(&mut self) {
        let _ = self.tx.sink_into(&mut self.stream);
    }
}

pub trait ConnectHandler<In, Out> = FnMut(&Server<In, Out>, &mut Client) -> std::io::Result<()>;
pub trait MessageHandler<In, Out> = FnMut(&Server<In, Out>, &mut Client, In) -> std::io::Result<()>;
pub trait TickHandler<In, Out> = FnMut(&Server<In, Out>) -> std::io::Result<()>;

#[derive(Default)]
struct Handlers<'srv, In, Out> {
    pub connect: Option<Box<dyn ConnectHandler<In, Out> + 'srv>>,
    pub message: Option<Box<dyn MessageHandler<In, Out> + 'srv>>,
    pub tick: Option<Box<dyn TickHandler<In, Out> + 'srv>>,
}

enum ContentType {
    Html,
    Js,
}

struct Resource {
    content_type: ContentType,
    path: String,
}

pub struct Server<'srv, In, Out> {
    poll: UnsafeCell<Poll>,
    clients: UnsafeCell<[Option<Client>; MAX_CLIENTS]>,
    sha1: UnsafeCell<Sha1>,
    handlers: UnsafeCell<Handlers<'srv, In, Out>>,
    resources: HashMap<String, Resource>,
    tick_interval: UnsafeCell<Option<Duration>>,
    next_tick: UnsafeCell<Option<Instant>>,
    marker: PhantomData<(In, Out)>,
}

impl<'srv, In: Default, Out: Default> Default for Server<'srv, In, Out> {
    fn default() -> Self {
        Self {
            poll: UnsafeCell::new(Poll::new().unwrap()),
            clients: UnsafeCell::new([const { None }; MAX_CLIENTS]),
            sha1: UnsafeCell::new(Sha1::new()),
            handlers: Default::default(),
            resources: HashMap::new(),
            tick_interval: None.into(),
            next_tick: None.into(),
            marker: PhantomData,
        }
    }
}

impl<'srv, In: Deserialize, Out: Serialize> Server<'srv, In, Out> {
    #[allow(clippy::mut_from_ref)]
    fn poll_mut(&self) -> &mut Poll {
        unsafe { &mut *self.poll.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn sha1_mut(&self) -> &mut Sha1 {
        unsafe { &mut *self.sha1.get() }
    }

    fn tick_interval(&self) -> &Option<Duration> {
        unsafe { &*self.tick_interval.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn tick_interval_mut(&self) -> &mut Option<Duration> {
        unsafe { &mut *self.tick_interval.get() }
    }

    fn next_tick(&self) -> &Option<Instant> {
        unsafe { &*self.next_tick.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn next_tick_mut(&self) -> &mut Option<Instant> {
        unsafe { &mut *self.next_tick.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn clients_mut(&self) -> &mut [Option<Client>; MAX_CLIENTS] {
        unsafe { &mut *self.clients.get() }
    }

    pub fn clients_iter_mut(&self) -> ClientIterMut<'srv> {
        let clients = self.clients_mut();
        ClientIterMut {
            ptr: unsafe { NonNull::from_ref(clients.first_mut().unwrap()).sub(1) },
            end: unsafe { NonNull::from_ref(clients.last_mut().unwrap()).add(1) },
            _marker: PhantomData,
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn handlers_mut(&self) -> &mut Handlers<'srv, In, Out> {
        unsafe { &mut *self.handlers.get() }
    }

    pub fn on_connect(&mut self, f: impl ConnectHandler<In, Out> + 'srv) {
        self.handlers_mut().connect = Some(Box::new(f));
    }

    pub fn on_message(&mut self, f: impl MessageHandler<In, Out> + 'srv) {
        self.handlers_mut().message = Some(Box::new(f));
    }

    pub fn on_tick(&mut self, interval: Duration, f: impl TickHandler<In, Out> + 'srv) {
        self.tick_interval_mut().replace(interval);
        self.handlers_mut().tick = Some(Box::new(f));
    }

    pub fn resource(&mut self, url: &str, path: &str) {
        let resource = if path.ends_with(".html") {
            Resource {
                content_type: ContentType::Html,
                path: path.to_owned(),
            }
        } else if path.ends_with(".js") {
            Resource {
                content_type: ContentType::Js,
                path: path.to_owned(),
            }
        } else {
            panic!()
        };
        self.resources.insert(url.to_owned(), resource);
    }

    pub fn run(&mut self) {
        const LISTENER: Token = Token(usize::MAX);

        let mut listener =
            TcpListener::bind("0.0.0.0:8000".parse::<SocketAddr>().unwrap()).unwrap();

        self.poll_mut()
            .registry()
            .register(&mut listener, LISTENER, Interest::READABLE)
            .unwrap();

        if let Some(tick_interval) = self.tick_interval() {
            self.next_tick_mut()
                .replace(Instant::now() + *tick_interval);
        }

        let mut events = Events::with_capacity(1024);
        let mut skip = None;
        'main_loop: loop {
            let mut timeout = *self.tick_interval();

            if let Some(next_tick) = self.next_tick()
                && !Instant::now().duration_since(*next_tick).is_zero()
            {
                let tick_begin = Instant::now();
                self.handlers_mut().tick.as_mut().unwrap()(self).unwrap(); // TODO
                let tick_elapsed = Instant::now() - tick_begin;
                let t = self.tick_interval().unwrap().saturating_sub(tick_elapsed);
                self.next_tick_mut().replace(Instant::now() + t);
                timeout = Some(t);
            }

            let iter = if let Some(skip) = skip {
                events.iter().skip(skip)
            } else {
                self.poll_mut().poll(&mut events, timeout).unwrap();
                #[allow(clippy::iter_skip_zero)]
                events.iter().skip(0)
            };

            for (i, event) in iter.enumerate() {
                if let Some(next_tick) = self.next_tick()
                    && !Instant::now().duration_since(*next_tick).is_zero()
                {
                    skip = Some(i);
                    continue 'main_loop;
                }

                match event.token() {
                    LISTENER => loop {
                        match listener.accept() {
                            Ok((mut stream, _addr)) => {
                                let mut client_idx = None;
                                for (idx, slot) in self.clients_mut().iter_mut().enumerate() {
                                    if slot.is_none() {
                                        client_idx = Some(idx);
                                        break;
                                    }
                                }

                                if let Some(client_idx) = client_idx {
                                    self.poll_mut()
                                        .registry()
                                        .register(
                                            &mut stream,
                                            Token(client_idx),
                                            Interest::READABLE | Interest::WRITABLE,
                                        )
                                        .unwrap();
                                    self.clients_mut()[client_idx] = Some(Client {
                                        id: client_idx,
                                        state: ClientState::PreHandshake,
                                        expect: Expect::Frame,
                                        stream,
                                        rx: Default::default(),
                                        tx: Default::default(),
                                        payload_len: 0,
                                        masking_key: [0; 4],
                                    });
                                } else {
                                    panic!()
                                }
                            }
                            Err(ref e) => match e.kind() {
                                std::io::ErrorKind::WouldBlock => {
                                    break;
                                }
                                _ => todo!(),
                            },
                        }
                    },
                    Token(client_idx) => {
                        let client = self.clients_mut()[client_idx].as_mut().unwrap();

                        if event.is_readable() {
                            loop {
                                match client.rx.source_from(&mut client.stream) {
                                    Ok(0) => {
                                        // socket closed
                                        self.poll_mut()
                                            .registry()
                                            .deregister(&mut client.stream)
                                            .unwrap();
                                        self.clients_mut()[client_idx].take();
                                        break;
                                    }
                                    Ok(_) => (),
                                    Err(ref e) => match e.kind() {
                                        std::io::ErrorKind::WouldBlock => {
                                            match client.state {
                                                ClientState::PreHandshake => {
                                                    match self
                                                        .serve_content(client, self.sha1_mut())
                                                    {
                                                        Ok(_) => (),
                                                        Err(_) => {
                                                            self.poll_mut()
                                                                .registry()
                                                                .deregister(&mut client.stream)
                                                                .unwrap();
                                                            self.clients_mut()[client_idx].take();
                                                        }
                                                    }
                                                }
                                                ClientState::PostHandshake => {
                                                    match self.handle_data(client) {
                                                        Ok(_) => (),
                                                        Err(_) => {
                                                            self.poll_mut()
                                                                .registry()
                                                                .deregister(&mut client.stream)
                                                                .unwrap();
                                                            self.clients_mut()[client_idx].take();
                                                        }
                                                    }
                                                }
                                            }

                                            break;
                                        }
                                        _ => {
                                            self.poll_mut()
                                                .registry()
                                                .deregister(&mut client.stream)
                                                .unwrap();
                                            self.clients_mut()[client_idx].take();
                                            break;
                                        }
                                    },
                                }
                            }
                        }

                        if event.is_writable() && matches!(client.state, ClientState::PostHandshake)
                        {
                            match client.tx.sink_into(&mut client.stream) {
                                Ok(_) => (),
                                Err(ref e) => match e.kind() {
                                    ErrorKind::WouldBlock | ErrorKind::Interrupted => (),
                                    _ => {
                                        self.clients_mut()[client_idx].take();
                                    }
                                },
                            }
                        }

                        if event.is_write_closed() || event.is_read_closed() || event.is_error() {
                            self.clients_mut()[client_idx].take();
                        }
                    }
                }
            }

            skip = None;
        }
    }

    fn serve_content(&self, client: &mut Client, sha1: &mut Sha1) -> std::io::Result<()> {
        println!("serving content");
        let mut req = String::new();
        if client.rx.read_to_string(&mut req).is_err() {
            return Ok(());
        }
        println!("{}", req);

        for line in req.lines() {
            if line.starts_with("Connection: Upgrade") {
                self.do_handshake(client, req, sha1);
                if let Some(f) = &mut self.handlers_mut().connect {
                    f(self, client)?;
                }

                client.state = ClientState::PostHandshake;
                return Ok(());
            }
        }

        let (file, content_type) = if let Some(path) = req.split_whitespace().nth(1) {
            println!("PATH: {path}");

            match self.resources.get(path) {
                Some(res) => match res.content_type {
                    ContentType::Html => (&res.path, "text/html; charset=UTF-8"),
                    ContentType::Js => (&res.path, "text/javascript; charset=UTF-8"),
                },
                None => {
                    client
                        .stream
                        .write_blocking("HTTP/1.1 404 Not Found\r\n\r\n".as_bytes())?;
                    return Ok(());
                }
            }
        } else {
            panic!()
        };

        let mut content = String::new();
        File::open(file)?.read_to_string(&mut content)?;
        let content_len = content.len();
        client.stream.write_blocking(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {content_len}\r\n\r\n{content}").as_bytes())?;
        Ok(())
    }

    fn do_handshake(&self, client: &mut Client, request: String, sha1: &mut Sha1) {
        let mut ws_key: Option<String> = None;
        for line in request.lines() {
            if line.starts_with("Sec-WebSocket-Key")
                && let Some((_, key)) = line.split_once(' ')
            {
                ws_key = Some(key.into());
            }
        }

        sha1.update(ws_key.unwrap());
        sha1.update("258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        let hash = BASE64_STANDARD.encode(sha1.finalize_reset());
        let response = str::from_utf8(hash.as_bytes()).unwrap();

        client.stream.write_blocking(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".as_bytes(),
    ).unwrap();
        client.stream.write_blocking(response.as_bytes()).unwrap();
        client.stream.write_blocking(b"\r\n\r\n").unwrap();
    }

    fn handle_data(&self, client: &mut Client) -> std::io::Result<()> {
        loop {
            match client.expect {
                Expect::Frame => {
                    let mut buffer: [u8; 2] = [0; 2];
                    match client.rx.read_exact(&mut buffer) {
                        Ok(_) => (),
                        Err(_) => return Ok(()),
                    }
                    if buffer[0] >> 7 & 1 != 1 {
                        panic!("got continuation frame");
                    }

                    let opcode = (buffer[0]) & 0b1111;
                    match opcode {
                        2 => (),
                        8 => {
                            return Err(Error::new(ErrorKind::ConnectionAborted, ""));
                        }
                        _ => todo!(),
                    }

                    let len = buffer[1] & 0b01111111;
                    if len < 126 {
                        client.payload_len = len as usize;
                        client.expect = Expect::MaskingKey;
                    } else if len == 126 {
                        client.expect = Expect::PayloadLen16;
                    } else {
                        client.expect = Expect::PayloadLen64;
                    };
                }
                Expect::PayloadLen16 => todo!(),
                Expect::PayloadLen64 => todo!(),
                Expect::MaskingKey => {
                    let mut buffer: [u8; 4] = [0; 4];
                    match client.rx.read_exact(&mut buffer) {
                        Ok(_) => {
                            client.masking_key = buffer;
                            client.expect = Expect::Payload;
                        }
                        Err(_) => return Ok(()),
                    }
                }
                Expect::Payload => {
                    if client.rx.len() < client.payload_len {
                        return Ok(());
                    }

                    let (lower, upper) = client.rx.as_mut_slices().unwrap();
                    // TODO simd
                    lower
                        .iter_mut()
                        .zip(client.masking_key.iter().cycle())
                        .for_each(|(x, k)| {
                            *x ^= k;
                        });
                    if let Some(upper) = upper {
                        upper
                            .iter_mut()
                            .zip(client.masking_key.iter().cycle())
                            .for_each(|(x, k)| {
                                *x ^= k;
                            });
                    }

                    if let Some(f) = &mut self.handlers_mut().message {
                        let msg = In::deserialize(&mut client.rx).unwrap();
                        f(self, client, msg)?;
                    }

                    client.expect = Expect::Frame;

                    if client.rx.is_empty() {
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

pub struct ClientIterMut<'srv> {
    ptr: NonNull<Option<Client>>,
    end: NonNull<Option<Client>>,
    _marker: PhantomData<&'srv mut Client>,
}

impl<'srv> Iterator for ClientIterMut<'srv> {
    type Item = &'srv mut Client;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            self.ptr = unsafe { self.ptr.add(1) };
            if self.ptr == self.end {
                return None;
            }

            if let Some(c) = unsafe { self.ptr.as_mut() } {
                match c.state {
                    ClientState::PreHandshake => {
                        continue;
                    }
                    ClientState::PostHandshake => {
                        return Some(c);
                    }
                }
            }
        }
    }
}
