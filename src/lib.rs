use crate::http::{default_headers, Headers, HttpHandler, HttpMethod, HttpRequest};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::{Read, Seek, SeekFrom};
use std::num::ParseIntError;
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;

mod headers;
pub mod http;

const DEFAULT_CHUNK_SIZE: usize = 5 * 1024 * 1024;

pub struct Client<'a> {
    use_method_override: bool,
    http_handler: Box<dyn HttpHandler + 'a>,
}

impl<'a> Client<'a> {
    pub fn new(http_handler: impl HttpHandler + 'a) -> Self {
        Client {
            use_method_override: false,
            http_handler: Box::new(http_handler),
        }
    }

    pub fn with_method_override(http_handler: impl HttpHandler + 'a) -> Self {
        Client {
            use_method_override: true,
            http_handler: Box::new(http_handler),
        }
    }

    /// Get the number of bytes already uploaded to the server
    pub fn get_progress(&self, url: &str) -> Result<ProgressResponse, Error> {
        let req = self.create_request(HttpMethod::Head, url, None, Some(default_headers()));

        let response = self.http_handler.deref().handle_request(req)?;

        let bytes_uploaded = response.headers.get_by_key(headers::UPLOAD_OFFSET);
        let total_size = response
            .headers
            .get_by_key(headers::UPLOAD_LENGTH)
            .and_then(|l| l.parse::<usize>().ok());

        if response.status_code.to_string().starts_with('4') || bytes_uploaded.is_none() {
            return Err(Error::NotFoundError);
        }

        let bytes_uploaded = bytes_uploaded.unwrap().parse()?;

        Ok(ProgressResponse {
            bytes_uploaded,
            total_size,
        })
    }

    pub fn upload(&self, url: &str, path: &Path) -> Result<(), Error> {
        self.upload_with_chunk_size(url, path, DEFAULT_CHUNK_SIZE)
    }

    pub fn upload_with_chunk_size(
        &self,
        url: &str,
        path: &Path,
        chunk_size: usize,
    ) -> Result<(), Error> {
        let progress = self.get_progress(url)?;
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();

        if let Some(total_size) = progress.total_size {
            if file_len as usize != total_size {
                return Err(Error::UnequalSizeError);
            }
        }

        let mut buffer = vec![0; chunk_size];
        let mut progress = progress.bytes_uploaded;

        file.seek(SeekFrom::Start(progress as u64))?;

        loop {
            let mut bytes_loaded = 0;
            loop {
                let mut end = bytes_loaded + 8000;
                if end > chunk_size {
                    end = chunk_size;
                }

                let bytes_read = file.read(&mut buffer[bytes_loaded..end])?;
                bytes_loaded += bytes_read;

                if bytes_read == 0 || end == chunk_size {
                    break;
                }
            }
            if buffer.is_empty() {
                return Err(Error::FileReadError);
            }

            let req = self.create_request(
                HttpMethod::Patch,
                url,
                Some(&buffer[..bytes_loaded]),
                Some(create_upload_headers(progress)),
            );

            let response = self.http_handler.deref().handle_request(req)?;

            if response.status_code == 409 {
                return Err(Error::WrongUploadOffsetError);
            }

            if response.status_code == 404 {
                return Err(Error::NotFoundError);
            }

            if response.status_code != 204 {
                return Err(Error::BadResponse(format!(
                    "Expected response status code to be '204', but received '{}'",
                    response.status_code
                )));
            }

            let upload_offset = match response.headers.get_by_key(headers::UPLOAD_OFFSET) {
                Some(offset) => Ok(offset),
                None => Err(Error::BadResponse(format!(
                    "'{}' header missing from response",
                    headers::UPLOAD_OFFSET
                ))),
            }?;

            progress = upload_offset.parse()?;

            if progress >= file_len as usize {
                break;
            }
        }

        Ok(())
    }

    /// Get information about the tus server
    pub fn get_server_info(&self, url: &str) -> Result<ServerInfo, Error> {
        let req = self.create_request(HttpMethod::Options, url, None, None);

        let response = self.http_handler.deref().handle_request(req)?;

        if ![200_usize, 204].contains(&response.status_code) {
            return Err(Error::BadResponse(format!(
                "Expected response status code to be '200' or '204', but received '{}'",
                response.status_code
            )));
        }

        let supported_versions: Vec<String> = response
            .headers
            .get_by_key(headers::TUS_VERSION)
            .unwrap()
            .split(',')
            .map(String::from)
            .collect();
        let extensions: Vec<TusExtension> =
            if let Some(ext) = response.headers.get_by_key(headers::TUS_EXTENSION) {
                ext.split(',')
                    .map(str::parse)
                    .filter(Result::is_ok)
                    .map(Result::unwrap)
                    .collect()
            } else {
                Vec::new()
            };
        let max_upload_size = response
            .headers
            .get_by_key(headers::TUS_MAX_SIZE)
            .and_then(|h| h.parse::<usize>().ok());

        Ok(ServerInfo {
            supported_versions,
            extensions,
            max_upload_size,
        })
    }

    fn create_request<'b>(
        &self,
        method: HttpMethod,
        url: &str,
        body: Option<&'b [u8]>,
        headers: Option<Headers>,
    ) -> HttpRequest<'b> {
        let mut headers = headers.unwrap_or_default();

        let method = if self.use_method_override {
            headers.insert(
                headers::X_HTTP_METHOD_OVERRIDE.to_owned(),
                method.to_string(),
            );
            HttpMethod::Post
        } else {
            method
        };

        HttpRequest {
            method,
            url: String::from(url),
            body,
            headers,
        }
    }
}

#[derive(Debug)]
pub struct ProgressResponse {
    pub bytes_uploaded: usize,
    pub total_size: Option<usize>,
}

#[derive(Debug)]
pub struct ServerInfo {
    pub supported_versions: Vec<String>,
    pub extensions: Vec<TusExtension>,
    pub max_upload_size: Option<usize>,
}

#[derive(Debug, PartialEq)]
pub enum TusExtension {
    Creation,
    Expiration,
    Checksum,
    Termination,
    Concatenation,
}

impl FromStr for TusExtension {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "creation" => Ok(TusExtension::Creation),
            "expiration" => Ok(TusExtension::Expiration),
            "checksum" => Ok(TusExtension::Checksum),
            "termination" => Ok(TusExtension::Termination),
            "concatenation" => Ok(TusExtension::Concatenation),
            _ => Err(()),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    NotFoundError,
    BadResponse(String),
    IoError(io::Error),
    ParsingError(ParseIntError),
    UnequalSizeError,
    FileReadError,
    WrongUploadOffsetError,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::IoError(e)
    }
}

impl From<ParseIntError> for Error {
    fn from(e: ParseIntError) -> Self {
        Error::ParsingError(e)
    }
}

trait HeaderMap {
    fn get_by_key(&self, key: &str) -> Option<&String>;
}

impl HeaderMap for HashMap<String, String> {
    fn get_by_key(&self, key: &str) -> Option<&String> {
        self.keys()
            .find(|k| k.to_lowercase().as_str() == key)
            .and_then(|k| self.get(k))
    }
}

fn create_upload_headers(progress: usize) -> Headers {
    let mut headers = default_headers();
    headers.insert(
        headers::CONTENT_TYPE.to_owned(),
        "application/offset+octet-stream".to_owned(),
    );
    headers.insert(headers::UPLOAD_OFFSET.to_owned(), progress.to_string());
    headers
}
