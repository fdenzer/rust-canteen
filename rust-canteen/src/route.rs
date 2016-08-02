extern crate regex;

use std::env;
use std::fs::File;
use std::error::Error;
use std::path::PathBuf;
use std::io::prelude::*;
use std::collections::HashMap;
use std::collections::HashSet;
use regex::Regex;

use request::*;
use response::*;

#[derive(PartialEq, Eq, Hash, Debug)]
pub enum ParamType {
    Integer,
    String,
    Float,
    Path,
}

pub struct Route {
    matcher:     Regex,
    methods:     HashSet<Method>,
    params:      HashMap<String, ParamType>,
    pub handler: fn(&Request) -> Response,
}

impl Route {
    pub fn new(path: &str, mlist: Vec<Method>, handler: fn(&Request) -> Response) -> Route {
        let re = Regex::new(r"^<(?:(int|str|float|path):)?([\w_][a-zA-Z0-9_]*)>$").unwrap();
        let parts: Vec<&str> = path.split('/').filter(|&s| s != "").collect();
        let mut matcher: String = String::from(r"^");
        let mut params: HashMap<String, ParamType> = HashMap::new();
        let mut methods: HashSet<Method> = HashSet::new();

        for m in mlist {
            methods.insert(m);
        }

        for part in parts {
            let chunk: String = match re.is_match(part) {
                true  => {
                    let mut rc = String::new();
                    let caps = re.captures(part).unwrap();
                    let param = caps.at(2).unwrap().clone();
                    let ptype: ParamType = match caps.at(1) {
                        Some(x)     => {
                            match x.as_ref() {
                                "int"   => ParamType::Integer,
                                "float" => ParamType::Float,
                                "path"  => ParamType::Path,
                                _       => ParamType::String,

                            }
                        }
                        None        => ParamType::String,
                    };

                    let mstr: String = match ptype {
                        ParamType::String  => String::from(r"(?:[^/])+"),
                        ParamType::Integer => String::from(r"-*[0-9]+"),
                        ParamType::Float   => String::from(r"-*[0-9]*[.]?[0-9]+"),
                        ParamType::Path    => String::from(r".+"),
                    };

                    rc.push_str("/(?P<");
                    rc.push_str(&param);
                    rc.push_str(">");
                    rc.push_str(&mstr);
                    rc.push_str(")");
                    params.insert(String::from(param), ptype);

                    rc
                },
                false => String::from("/") + part,
            };

            matcher.push_str(&chunk);
        }

        /* end the regex with an optional final slash and a string terminator */
        matcher.push_str("/?$");

        Route {
            matcher: Regex::new(&matcher).unwrap(),
            params:  params,
            methods: methods,
            handler: handler,
        }
    }

    pub fn is_match(&self, req: &Request) -> bool {
        self.matcher.is_match(&req.path) && self.methods.contains(&req.method)
    }

    pub fn parse(&self, path: &str) -> Option<HashMap<String, String>> {
        let mut params: HashMap<String, String> = HashMap::new();

        return match self.matcher.is_match(&path) {
            true  => {
                let caps = self.matcher.captures(path).unwrap();
                for (param, _) in &self.params {
                    params.insert(param.clone(), String::from(caps.name(&param).unwrap()));
                }

                Some(params)
            },
            false => None,
        }
    }

    pub fn err_403(req: &Request) -> Response {
        Response::err_403(&req.path)
    }

    pub fn err_404(req: &Request) -> Response {
        Response::err_404(&req.path)
    }

    pub fn static_file(req: &Request) -> Response {
        let mut res = Response::new();

        let cwd = env::current_dir().unwrap();
        let clean = Route::replace_escape(&req.path);
        let mut fpath = PathBuf::from(&cwd);
        let mut fbuf: Vec<u8> = Vec::new();

        for chunk in clean.split('/') {
            if chunk == "" || chunk == "." || chunk == ".." {
                /* bzzzzt */
                continue;
            }

            fpath.push(&chunk);
        }

        let file = File::open(&fpath);

        match file {
            Ok(mut f)   => {
                match f.read_to_end(&mut fbuf) {
                    Ok(_)   => {
                        res.set_code(200);
                        res.set_content_type("text/plain");
                        res.append(fbuf);
                    },
                    Err(e)  => {
                        return Response::err_500(e.description());
                    },
                }
            },
            Err(_)      => {
                return Response::err_404(&req.path);
            }
        }

        res
    }
}
