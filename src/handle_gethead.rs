use std::io::prelude::*;
use std::io::BufWriter;

use htmlescape;
use http::status::StatusCode;
use time;

use crate::sync_adapter::{Request,Response};
use crate::typed_headers::{self,ByteRangeSpec,HeaderMapExt};

use crate::fs::*;
use crate::errors::DavError;
use crate::webpath::WebPath;
use crate::headers;
use crate::conditional;
use crate::{fserror,statuserror,systemtime_to_httpdate,systemtime_to_timespec};

impl crate::DavInner {
    pub(crate) fn handle_get(&self, req: Request, mut res: Response) -> Result<(), DavError> {

        let head = req.method == http::Method::HEAD;

        // check if it's a directory.
        let path = self.path(&req);
        let meta = self.fs.metadata(&path).map_err(|e| fserror(&mut res, e))?;
        if meta.is_dir() {
            return self.handle_dirlist(req, res, &path, head);
        }

        // double check, is it a regular file.
        let mut file = self.fs.open(&path, OpenOptions::read()).map_err(|e| fserror(&mut res, e))?;
        let meta = file.metadata().map_err(|e| fserror(&mut res, e))?;
        if !meta.is_file() {
            return Err(statuserror(&mut res, StatusCode::METHOD_NOT_ALLOWED));
        }

        let mut start = 0;
        let mut count = meta.len();
        let len = count;
        let mut do_range = true;

        let file_etag = typed_headers::EntityTag::new(false, meta.etag());

        if let Some(r) = req.headers.typed_get::<headers::IfRange>() {
            do_range = conditional::ifrange_match(&r, &file_etag, meta.modified().unwrap());
        }

        // see if we want to get a range.
        if do_range {
            do_range = false;
            if let Some(r) = req.headers.typed_get::<typed_headers::Range>() {
                match r {
                    typed_headers::Range::Bytes(ref ranges) => {
                        // we only support a single range
                        if ranges.len() == 1 {
                            match &ranges[0] {
                                &ByteRangeSpec::FromTo(s, e) => {
                                    start = s; count = e - s + 1;
                                },
                                &ByteRangeSpec::AllFrom(s) => {
                                    start = s; count = len - s;
                                },
                                &ByteRangeSpec::Last(n) => {
                                    start = len - n; count = n;
                                },
                            }
                            if start >= len {
                                return Err(statuserror(&mut res, StatusCode::RANGE_NOT_SATISFIABLE));
                            }
                            if start + count > len {
                                count = len - start;
                            }
                            do_range = true;
                        }
                    },
                    _ => {},
                }
            }
        }

        // set Last-Modified and ETag headers.
        if let Ok(modified) = meta.modified() {
            res.headers_mut().typed_insert(typed_headers::LastModified(
                    systemtime_to_httpdate(modified)));
        }
        res.headers_mut().typed_insert(typed_headers::ETag(file_etag));

        // handle the if-headers.
        if let Some(s) = conditional::if_match(&req,Some(&meta), &self.fs, &self.ls, &path) {
            return Err(statuserror(&mut res, s));
        }

        if do_range {
            // seek to beginning of requested data.
            if let Err(_) = file.seek(std::io::SeekFrom::Start(start)) {
                *res.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                return Ok(());
            }

            // set partial-content status and add content-range header.
            let r = format!("bytes {}-{}/{}", start, start + count - 1, len);
            res.headers_mut().insert("Content-Range", r.parse().unwrap());
            *res.status_mut() = StatusCode::PARTIAL_CONTENT;
        } else {
            // normal request, send entire file.
            *res.status_mut() = StatusCode::OK;
        }

        // set content-length and start.
        res.headers_mut().insert("Content-Type", path.get_mime_type_str().parse().unwrap());
        res.headers_mut().typed_insert(typed_headers::ContentLength(count));
        res.headers_mut().typed_insert(typed_headers::AcceptRanges(vec![typed_headers::RangeUnit::Bytes]));

        if head {
            return Ok(())
        }

        // now just loop and send data.
        let mut writer = res.start();

        let mut buffer = [0; 8192];
        let zero = [0; 4096];

        while count > 0 {
            let data;
            let mut n = file.read(&mut buffer[..])?;
            if n > count as usize {
                n = count as usize;
            }
            if n == 0 {
                // this is a cop out. if the file got truncated, just
                // return zero bytes instead of file content.
                n = if count > 4096 { 4096 } else { count as usize };
                data = &zero[..n];
            } else {
                data = &buffer[..n];
            }
            count -= n as u64;
            writer.write_all(data)?;
        }
        Ok(())
    }

    pub(crate) fn handle_dirlist(&self, _req: Request, mut res: Response, path: &WebPath, head: bool) -> Result<(), DavError> {

        // This is a directory. If the path doesn't end in "/", send a redir.
        // Most webdav clients handle redirect really bad, but a client asking
        // for a directory index is usually a browser.
        if !path.is_collection() {
            let mut path = path.clone();
            path.add_slash();
            res.headers_mut().insert("Location", path.as_utf8_string_with_prefix().parse().unwrap());
            res.headers_mut().typed_insert(typed_headers::ContentLength(0));
            *res.status_mut() = StatusCode::FOUND;
            return Ok(());
        }

        // read directory or bail.
        let entries = self.fs.read_dir(path).map_err(|e| fserror(&mut res, e))?;

        // start output
        res.headers_mut().insert("Content-Type", "text/html; charset=utf-8".parse().unwrap());
        *res.status_mut() = StatusCode::OK;
        if head {
            return Ok(())
        }
        let mut w = BufWriter::new(res.start());

        // transform all entries into a dirent struct.
        struct Dirent {
            path:       String,
            name:       String,
            meta:       Box<DavMetaData>,
        }
        let mut dirents = Vec::new();

        for dirent in entries {
            let mut name = dirent.name();
            if name.starts_with(b".") {
                continue;
            }
            let mut npath = path.clone();
            npath.push_segment(&name);
            let meta = match dirent.is_symlink() {
                Ok(v) if v == true => {
                    self.fs.metadata(&npath)
                },
                _ => {
                    dirent.metadata()
                },
            };
            if let Ok(meta) = meta {
                if meta.is_dir() {
                    name.push(b'/');
                    npath.add_slash();
                }
                dirents.push(Dirent{
                    path:   npath.as_url_string_with_prefix(),
                    name:   String::from_utf8_lossy(&name).to_string(),
                    meta:   meta,
                });
            }
        }

        // now we can sort the dirent struct.
        dirents.sort_by(|a, b| {
            let adir = a.meta.is_dir();
            let bdir = b.meta.is_dir();
            if adir && !bdir {
                std::cmp::Ordering::Less
            } else if bdir && !adir {
                std::cmp::Ordering::Greater
            } else {
                (a.name).cmp(&b.name)
            }
        });

        // and output html
        let upath = htmlescape::encode_minimal(&path.as_url_string());
        writeln!(w, "<html><head>")?;
        writeln!(w, "<title>Index of {}</title>", upath)?;
        writeln!(w, "<style>")?;
        writeln!(w, "table {{")?;
        writeln!(w, "  border-collapse: separate;")?;
        writeln!(w, "  border-spacing: 1.5em 0.25em;")?;
        writeln!(w, "}}")?;
        writeln!(w, "h1 {{")?;
        writeln!(w, "  padding-left: 0.3em;")?;
        writeln!(w, "}}")?;
        writeln!(w, ".mono {{")?;
        writeln!(w, "  font-family: monospace;")?;
        writeln!(w, "}}")?;
        writeln!(w, "</style>")?;
        writeln!(w, "</head>")?;

        writeln!(w, "<body>")?;
        writeln!(w, "<h1>Index of {}</h1>", upath)?;
        writeln!(w, "<table>")?;
        writeln!(w, "<tr>")?;
        writeln!(w, "<th>Name</th><th>Last modified</th><th>Size</th>")?;
        writeln!(w, "<tr><th colspan=\"3\"><hr></th></tr>")?;
        writeln!(w, "<tr><td><a href=\"..\">Parent Directory</a></td><td>&nbsp;</td><td class=\"mono\" align=\"right\">[DIR]</td></tr>")?;

        for dirent in &dirents {
            let modified = match dirent.meta.modified() {
                Ok(t) => {
                    let tm = time::at(systemtime_to_timespec(t));
                        format!("{:04}-{:02}-{:02} {:02}:{:02}",
                            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
                    },
                Err(_) => "".to_string(),
            };
            let size = match dirent.meta.is_file() {
                true => dirent.meta.len().to_string(),
                false => "[DIR]".to_string(),
            };
            let name = htmlescape::encode_minimal(&dirent.name);
            writeln!(w, "<tr><td><a href=\"{}\">{}</a></td><td class=\"mono\">{}</td><td class=\"mono\" align=\"right\">{}</td></tr>",
                     dirent.path, name, modified, size)?;
        }

        writeln!(w, "<tr><th colspan=\"3\"><hr></th></tr>")?;
        writeln!(w, "</table></body></html>")?;

        Ok(())
    }
}
