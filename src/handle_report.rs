use std::io::Cursor;

use headers::HeaderMapExt;
use http::{Request, Response, StatusCode};

use crate::davpath::DavPath;
use crate::handle_props::PropWriter;
use crate::xmltree_ext::*;
use xmltree::Element;

use crate::async_stream::AsyncStream;
use crate::body::Body;
use crate::davheaders;
use crate::errors::*;
use crate::util::dav_xml_error;
use crate::{DavInner, DavResult};

impl DavInner {
    pub(crate) async fn handle_report(
        self,
        req: &Request<()>,
        xmldata: &[u8],
    ) -> DavResult<Response<Body>> {
        // No checks on If: and If-* headers here, because I do not see
        // the point and there's nothing in RFC4918 that indicates we should.

        let mut res = Response::new(Body::empty());

        res.headers_mut()
            .typed_insert(headers::CacheControl::new().with_no_cache());
        res.headers_mut().typed_insert(headers::Pragma::no_cache());

        let depth = match req.headers().typed_get::<davheaders::Depth>() {
            Some(davheaders::Depth::Infinity) | None => {
                if req.headers().typed_get::<davheaders::XLitmus>().is_none() {
                    let ct = "application/xml; charset=utf-8".to_owned();
                    res.headers_mut().typed_insert(davheaders::ContentType(ct));
                    *res.status_mut() = StatusCode::FORBIDDEN;
                    *res.body_mut() = dav_xml_error("<D:propfind-finite-depth/>");
                    return Ok(res);
                }
                davheaders::Depth::Infinity
            }
            Some(d) => d,
        };

        // path and meta
        let mut path = self.path(req);

        let mut root = None;
        if !xmldata.is_empty() {
            trace!("{}", String::from_utf8(xmldata.to_vec()).unwrap());
            root = match Element::parse(Cursor::new(xmldata)) {
                Ok(t) => {
                    // For now, Just supporting addressbook-multiget 
                    if t.name == "addressbook-multiget" && t.namespace.as_deref() == Some("urn:ietf:params:xml:ns:carddav") {
                        Some(t)
                    } else {
                        return Err(DavError::XmlParseError);
                    }
                }
                Err(_) => return Err(DavError::XmlParseError),
            };
        }

        let (name, props) = match root.clone() {
            None => ("allprop", Vec::new()),
            Some(elem) => {
                match elem
                    .child_elems_into_iter()
                    .find(|e| e.name == "prop")
                {
                    Some(elem) => match elem.name.as_str() {
                        "prop" => ("prop", elem.take_child_elems()),
                        _ => return Err(DavError::XmlParseError),
                    },
                    None => return Err(DavError::XmlParseError),
                }
            }
        };

        let list_href = match root {
            None => Vec::new(),
            Some(elem) => elem
                .take_child_elems()
                .into_iter()
                .filter(|e| e.name == "href")
                .collect()
        };

        trace!("report: type request: {}", name);

        let mut pw = PropWriter::new(req, &mut res, name, props, &self.fs, self.ls.as_ref())?;

        *res.body_mut() = Body::from(AsyncStream::new(|tx| async move {
            pw.set_tx(tx);
            if depth != davheaders::Depth::Zero {
                for e in list_href.iter() {
                    let url = e.get_text().unwrap().into_owned();
                    let url = DavPath::from_str_and_prefix(&url, "")
                        .map_err(|_| DavError::InvalidPath)?;
                    
                    // In report, we want to change d:href path and translate it into our own path
                    let url = self.fs.patch_path(&url).await?;

                    // Write contacts file from url
                    let meta = self.fs.metadata(&url).await?;
                    pw.write_props(&url, meta).await?;
                    pw.flush().await?;
                    
                }
            }
            pw.close().await?;

            Ok(())
        }));
        
        Ok(res)
    }    
}