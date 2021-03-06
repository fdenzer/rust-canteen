// Copyright (c) 2016
// Jeff Nettleton
//
// Licensed under the MIT license (http://opensource.org/licenses/MIT). This
// file may not be copied, modified, or distributed except according to those
// terms

//! # Canteen
//!
//! ## Description
//!
//! A pure Rust clone of [Flask](http://flask.pocoo.org), a simple but powerful Python
//! web framework.
//!
//! The principle behind Canteen is simple -- handler functions are defined as simple
//! Rust functions that take a `Request` and return a `Response`. Handlers are then attached
//! to one or more routes and HTTP methods/verbs. Routes are specified using a simple
//! syntax that lets you define variables within them; variables that can then be
//! extracted to perform various operations. Currently, the following variable types can
//! be used:
//!
//! - `<str:name>` will match anything inside a path segment, returns a `String`
//! - `<int:name>` will return a signed integer (`i32`) from a path segment
//!   - ex: `cnt.add_route("/api/foo/<int:foo_id>", &[Method::Get], my_handler)` will match
//!   `"/api/foo/123"` but not `"/api/foo/123.34"` or `"/api/foo/bar"`
//! - `<uint:name>` will return an unsigned integer (`u32`)
//! - `<float:name>` does the same thing as the `int` parameter definition, but matches numbers
//! with decimal points and returns an `f32`
//! - `<path:name>` will greedily take all path data contained, returns a `String`
//!   - ex: `cnt.add_route("/static/<path:name>", &[Method::Get], utils::static_file)` will
//!   serve anything in the `/static/` directory as a file
//!
//! After the handlers are attached to routes, the next step is to simply start the
//! server. Any time a request is received, it is dispatched with the associated handler
//! to a threadpool worker. The worker notifies the parent process when it's finished,
//! and then the response is transmitted back to the client. Pretty straightforward stuff!
//!
//! ## Example
//!
//! ```rust
//! extern crate canteen;
//!
//! use canteen::{Canteen, Request, Response, Method};
//! use canteen::utils;
//!
//! fn hello_handler(_: &Request) -> Response {
//!     let mut res = Response::new();
//!
//!     res.set_status(200);
//!     res.set_content_type("text/plain");
//!     res.append("Hello, world!");
//!
//!     res
//! }
//!
//! fn double_handler(req: &Request) -> Response {
//!     let to_dbl: i32 = req.get("to_dbl");
//!
//!     /* simpler response generation syntax */
//!     utils::make_response(format!("{}", to_dbl * 2), "text/plain", 200)
//! }
//!
//! fn main() {
//!     let mut cnt = Canteen::new();
//!
//!     // bind to an address
//!     cnt.bind(("127.0.0.1", 8888));
//!
//!     // set the default route handler to show a 404 message
//!     cnt.set_default(utils::err_404);
//!
//!     // respond to requests to / with "Hello, world!"
//!     cnt.add_route("/", &[Method::Get], hello_handler);
//!
//!     // pull a variable from the path and do something with it
//!     cnt.add_route("/double/<int:to_dbl>", &[Method::Get], double_handler);
//!
//!     // serve raw files from the /static/ directory
//!     cnt.add_route("/static/<path:path>", &[Method::Get], utils::static_file);
//! }
//! ```

pub mod utils;
pub mod route;
pub mod request;
pub mod response;

#[cfg(test)]
#[macro_use]
extern crate serde_derive;

use std::str::FromStr;
use std::io::Result;
use std::io::prelude::*;
use std::net::ToSocketAddrs;
use std::collections::HashMap;
use std::collections::HashSet;

use threadpool::ThreadPool;
use mio::tcp::{TcpListener, TcpStream};
use mio::util::Slab;
use mio::*;

pub use crate::request::*;
pub use crate::response::*;

struct Client {
    sock:   TcpStream,
    token:  Token,
    events: EventSet,
    i_buf:  Vec<u8>,
    o_buf:  Vec<u8>,
}

impl Client {
    fn new(sock: TcpStream, token: Token) -> Client {
        Client {
            sock,
            token,
            events: EventSet::hup(),
            i_buf:  Vec::with_capacity(2048),
            o_buf:  Vec::new(),
        }
    }

    fn receive(&mut self) -> Result<bool> {
        let mut bytes_read: usize = 0;

        loop {
            let mut buf: Vec<u8> = Vec::with_capacity(2048);
            match self.sock.try_read_buf(&mut buf) {
                Ok(size)  => {
                    match size {
                        Some(bytes) => {
                            self.i_buf.extend(buf);
                            bytes_read += bytes;
                        },
                        None    => {
                            self.events.remove(EventSet::readable());
                            self.events.insert(EventSet::writable());
                            break;
                        },
                    }
                },
                Err(_)  => {
                    self.events.remove(EventSet::readable());
                    self.events.insert(EventSet::writable());
                    break;
                },
            };
        }

        Ok(bytes_read > 0)
    }

    // write the client's output buffer to the socket.
    //
    // the following return values mean:
    //  - Ok(true):  we can close the connection
    //  - Ok(false): keep listening for writeable event and continue next time
    //  - Err(e):    something dun fucked up
    fn send(&mut self) -> Result<bool> {
        if self.o_buf.is_empty() {
            return Ok(false);
        }

        while !self.o_buf.is_empty() {
            match self.sock.write(&self.o_buf.as_slice()) {
                Ok(sz)  => {
                    if sz == self.o_buf.len() {
                        // we did it!
                        self.events.remove(EventSet::writable());
                        break;
                    } else {
                        // keep going
                        self.o_buf = self.o_buf.split_off(sz);
                    }
                },
                Err(_)  => {
                    return Ok(true);
                }
            }
        }

        Ok(true)
    }

    fn register(&mut self, evl: &mut EventLoop<Canteen>) -> Result<()> {
        self.events.insert(EventSet::readable());
        evl.register(&self.sock, self.token, self.events, PollOpt::edge() | PollOpt::oneshot())
    }

    fn reregister(&mut self, evl: &mut EventLoop<Canteen>) -> Result<()> {
        evl.reregister(&self.sock, self.token, self.events, PollOpt::edge() | PollOpt::oneshot())
    }
}

/// The primary struct provided by the library. The aim is to have a similar
/// interface to Flask, the Python microframework.
pub struct Canteen {
    routes:  HashMap<route::RouteDef, route::Route>,
    rcache:  HashMap<route::RouteDef, route::RouteDef>,
    server:  Option<TcpListener>,
    token:   Token,
    conns:   Slab<Client>,
    default: fn(&Request) -> Response,
    tpool:   ThreadPool,
}

impl Handler for Canteen {
    type Timeout = ();
    type Message = (Token, Vec<u8>);

    fn ready(&mut self, evl: &mut EventLoop<Canteen>, token: Token, events: EventSet) {
        if events.is_error() || events.is_hup() {
            self.reset_connection(token);
            return;
        }

        if events.is_readable() {
            if self.token == token {
                let sock = self.accept().unwrap();

                if let Some(token) = self.conns.insert_with(|token| Client::new(sock, token)) {
                    self.get_client(token).register(evl).ok();
                }

                self.reregister(evl);
            } else {
                self.readable(evl, token)
                    .and_then(|_| self.get_client(token)
                                      .reregister(evl)).ok();
            }

            return;
        }

        if events.is_writable() {
            match self.get_client(token).send() {
                Ok(true)    => { self.reset_connection(token); },
                Ok(false)   => { let _ = self.get_client(token).reregister(evl); },
                Err(_)      => {},
            }
        }
    }

    fn notify(&mut self, evl: &mut EventLoop<Canteen>, msg: (Token, Vec<u8>)) {
        let (token, output) = msg;
        let client = self.get_client(token);

        client.o_buf = output;
        let _ = client.reregister(evl);
    }
}

impl Canteen {
    /// Creates a new Canteen instance.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use canteen::Canteen;
    ///
    /// let cnt = Canteen::new();
    /// ```
    pub fn new() -> Canteen {
        Canteen {
            routes:  HashMap::new(),
            rcache:  HashMap::new(),
            server:  None,
            token:   Token(1),
            conns:   Slab::new_starting_at(Token(2), 2048),
            default: utils::err_404,
            tpool:   ThreadPool::new(255),
        }
    }

    /// Bind to an address on which to listen for connections
    /// # Examples
    /// ```rust,ignore
    /// use canteen::Canteen;
    ///
    /// let mut cnt = Canteen::new();
    /// cnt.bind(("127.0.0.1", 8080));
    /// ```
    pub fn bind<A: ToSocketAddrs>(&mut self, addr: A) {
        self.server = Some(TcpListener::bind(&addr.to_socket_addrs().unwrap().next().unwrap()).unwrap());
    }


    /// Adds a new route definition to be handled by Canteen.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use canteen::{Canteen, Request, Response, Method};
    /// use canteen::utils;
    ///
    /// fn handler(_: &Request) -> Response {
    ///     utils::make_response("<b>Hello, world!</b>", "text/html", 200)
    /// }
    ///
    /// fn main() {
    ///     let mut cnt = Canteen::new();
    ///     cnt.add_route("/hello", &[Method::Get], handler);
    /// }
    /// ```
    pub fn add_route(&mut self, path: &str, mlist: &[Method],
                     handler: fn(&Request) -> Response) -> &mut Canteen {
        let mut methods: HashSet<Method> = HashSet::new();

        // make them unique
        for m in mlist {
            methods.insert(*m);
        }

        for m in methods {
            let rd = route::RouteDef {
                pathdef: String::from(path),
                method:  m,
            };

            if self.routes.contains_key(&rd) {
                panic!("a route handler for {} has already been defined!", path);
            }

            self.routes.insert(rd, route::Route::new(&path, m, handler));
        }

        self
    }

    /// Defines a default route for undefined paths.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use canteen::Canteen;
    /// use canteen::utils;
    ///
    /// let mut cnt = Canteen::new();
    /// cnt.set_default(utils::err_404);
    /// ```
    pub fn set_default(&mut self, handler: fn(&Request) -> Response) -> &mut Canteen {
        self.default = handler;

        self
    }

    fn get_client(&mut self, token: Token) -> &mut Client {
        self.conns.get_mut(token).unwrap()
    }

    fn accept(&mut self) -> Result<TcpStream> {
        if let Some(ref server) = self.server {
            if let Ok(s) = server.accept() {
                if let Some((sock, _)) = s {
                    return Ok(sock);
                }
            }
        }

        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "connection aborted prematurely".to_string()
        ))
    }

    fn handle_request(&mut self, token: Token, tx: Sender<(Token, Vec<u8>)>, rqstr: &str) {
        let mut req = Request::from_str(&rqstr).unwrap();
        let mut handler: fn(&Request) -> Response = self.default;
        let resolved = route::RouteDef {
            pathdef: req.path.clone(),
            method:  req.method,
        };

        if self.rcache.contains_key(&resolved) {
            let route = &self.routes[&self.rcache[&resolved]];

            handler = route.handler;
            req.params = route.parse(&req.path);
        } else {
            for (path, route) in &self.routes {
                if route.is_match(&req) {
                    handler = route.handler;
                    req.params = route.parse(&req.path);
                    self.rcache.insert(resolved, (*path).clone());
                    break;
                }
            }
        }

        self.tpool.execute(move || {
            let _ = tx.send((token, handler(&req).gen_output()));
        });
    }

    fn readable(&mut self, evl: &mut EventLoop<Canteen>, token: Token) -> Result<bool> {
        if let Ok(true) = self.get_client(token).receive() {
            let buf = self.get_client(token).i_buf.clone();
            if let Ok(rqstr) = String::from_utf8(buf) {
                self.handle_request(token, evl.channel(), &rqstr);
            } else {
                return Ok(false);
            }
        }

        Ok(true)
    }

    fn reset_connection(&mut self, token: Token) {
        // kill the connection
        self.conns.remove(token);
    }

    fn register(&mut self, evl: &mut EventLoop<Canteen>) -> Result<()> {
        if let Some(ref server) = self.server {
            return evl.register(server, self.token, EventSet::readable(), PollOpt::edge() | PollOpt::oneshot());
        }

        Ok(())
    }

    fn reregister(&mut self, evl: &mut EventLoop<Canteen>) {
        if let Some(ref server) = self.server {
            evl.reregister(server, self.token,
                                 EventSet::readable(),
                                 PollOpt::edge() | PollOpt::oneshot()).ok();
        }
    }

    /// Creates the listener and starts a Canteen server's event loop.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use canteen::Canteen;
    ///
    /// let mut cnt = Canteen::new();
    /// cnt.run();
    /// ```
    pub fn run(&mut self) {
        let mut evl = match EventLoop::new() {
            Ok(event_loop)  => event_loop,
            Err(_)          => panic!("unable to initiate event loop"),
        };

        match self.server {
            None    => println!("server not bound to an address!"),
            Some(_) => {
                self.register(&mut evl).ok();
                evl.run(self).unwrap();
            },
        };
    }
}

impl Default for Canteen {
    fn default() -> Self {
        Canteen::new()
    }
}
