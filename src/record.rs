use chrono::prelude::*;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use uuid::Uuid;

use crate::header::WarcHeader;
use crate::record_type::RecordType;
use crate::truncated_type::TruncatedType;
use crate::Error as WarcError;

pub use streaming_trait::BufferedBody;
use streaming_trait::StreamingType;

mod streaming_trait {
    use std::io::Read;

    /// A tag indicating how the body is stored within a record.
    pub trait StreamingType: Clone {}

    #[derive(Clone, Debug, PartialEq)]
    /// A tag indicating the body is stored in a buffer within the record.
    pub struct BufferedBody(pub Vec<u8>);
    impl StreamingType for BufferedBody {}

    #[derive(Clone)]
    /// A tag indicating the body is streamed from a reader.
    pub struct StreamingBody<T: Read + Clone>(T);
    impl<T: Read + Clone> StreamingType for StreamingBody<T> {}
}

/// A header block of a single WARC record as parsed from a data stream.
///
/// It is guaranteed to be well-formed, but may not be valid according to the specification.
#[derive(Clone, Debug, PartialEq)]
pub struct RawHeaderBlock {
    /// The WARC standard version this record reports conformance to.
    pub version: String,
    /// All headers that are part of this record.
    pub headers: HashMap<WarcHeader, Vec<u8>>,
}

impl AsRef<HashMap<WarcHeader, Vec<u8>>> for RawHeaderBlock {
    fn as_ref(&self) -> &HashMap<WarcHeader, Vec<u8>> {
        &self.headers
    }
}

impl AsMut<HashMap<WarcHeader, Vec<u8>>> for RawHeaderBlock {
    fn as_mut(&mut self) -> &mut HashMap<WarcHeader, Vec<u8>> {
        &mut self.headers
    }
}

impl std::fmt::Display for RawHeaderBlock {
    fn fmt(&self, w: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        for (key, value) in self.as_ref().iter() {
            writeln!(
                w,
                "{}: {}",
                key.to_string(),
                String::from_utf8_lossy(&value)
            )?;
        }
        Ok(())
    }
}

/// A data structure used for the formation of WARC records.
///
/// The resulting record is guaranteed to be well-formed, but may not be valid according to the
/// specification.
#[derive(Clone, Debug, PartialEq)]
pub struct RawRecord {
    /// The headers on this record.
    pub headers: RawHeaderBlock,
    /// The data body of this record.
    pub body: Vec<u8>,
}

/// A builder for WARC records from data.
#[derive(Clone, Default)]
pub struct RecordBuilder {
    value: Record<BufferedBody>,
    broken_headers: HashMap<WarcHeader, Vec<u8>>,
    last_error: Option<WarcError>,
}

/// A single WARC record.
///
/// It is guaranteed to be valid according to the specification it conforms to, except:
/// * The validity of the WARC-Record-ID header is not checked
/// * Date information not in the UTC timezone will be silently converted to UTC
///
/// This record can be constructed by a `RecordBuilder` or by a fallable cast from a `RawRecord`.
#[derive(Clone, Debug, PartialEq)]
pub struct Record<T: StreamingType> {
    // NB: invariant: does not contain the headers stored in the struct
    headers: RawHeaderBlock,
    record_date: DateTime<Utc>,
    record_id: String,
    record_type: RecordType,
    truncated_type: Option<TruncatedType>,
    body: T,
}

impl Record<BufferedBody> {
    /// Create a new empty record with default values.
    ///
    /// Using a `RecordBuilder` is more efficient when creating records from known data.
    ///
    /// A default record contains an empty body, and the following fields:
    /// * WARC-Record-ID: generated by `generate_record_id()`
    /// * WARC-Date: the current moment in time
    /// * WARC-Type: resource
    /// * WARC-Content-Length: 0
    pub fn new() -> Record<BufferedBody> {
        Record::default()
    }

    /// Transform this record into a raw record containing the same data.
    pub fn into_raw_parts(self) -> (RawHeaderBlock, Vec<u8>) {
        let Record {
            mut headers,
            record_date,
            record_id,
            record_type,
            body,
            ..
        } = self;
        let insert1 = headers.as_mut().insert(
            WarcHeader::ContentLength,
            format!("{}", body.0.len()).into(),
        );
        let insert2 = headers
            .as_mut()
            .insert(WarcHeader::WarcType, record_type.to_string().into());
        let insert3 = headers
            .as_mut()
            .insert(WarcHeader::RecordID, record_id.into());
        let insert4 = if let Some(ref truncated_type) = self.truncated_type {
            headers
                .as_mut()
                .insert(WarcHeader::Truncated, truncated_type.to_string().into())
        } else {
            None
        };
        let insert5 = headers.as_mut().insert(
            WarcHeader::Date,
            record_date
                .to_rfc3339_opts(SecondsFormat::Secs, true)
                .into(),
        );

        debug_assert!(
            insert1.is_none()
                && insert2.is_none()
                && insert3.is_none()
                && insert4.is_none()
                && insert5.is_none(),
            "invariant violation: raw struct contains externally stored fields"
        );

        (headers, body.0)
    }

    /// Generate and return a new value suitable for use in the WARC-Record-ID header.
    ///
    /// # Compatibility
    /// The standard only places a small number of constraints on this field:
    /// 1. This value is globally unique "for its period of use"
    /// 1. This value is a valid URI
    /// 1. This value "clearly indicate\[s\] a documented and registered scheme to which it conforms."
    ///
    /// These guarantees will be upheld by all generated outputs, where the "period of use" is
    /// presumed to be indefinite and unlimited.
    ///
    /// However, any *specific algorithm* used to generate values is **not** part of the crate's
    /// public API for purposes of semantic versioning.
    ///
    /// # Implementation
    /// The current implementation generates random values based on UUID version 4.
    ///
    pub fn generate_record_id() -> String {
        format!("<{}>", Uuid::new_v4().to_urn().to_string())
    }

    fn parse_content_length(len: &str) -> Result<u64, WarcError> {
        (len).parse::<u64>().map_err(|_| {
            WarcError::MalformedHeader(
                WarcHeader::ContentLength,
                "not an integer between 0 and 2^64-1".to_string(),
            )
        })
    }

    fn parse_record_date(date: &str) -> Result<DateTime<Utc>, WarcError> {
        DateTime::parse_from_rfc3339(date)
            .map_err(|_| {
                WarcError::MalformedHeader(
                    WarcHeader::Date,
                    "not an ISO 8601 datestamp".to_string(),
                )
            })
            .map(|date| date.into())
    }

    /// Return the Content-Length header for this record.
    ///
    /// This value is guaranteed to match the actual length of the body.
    pub fn content_length(&self) -> u64 {
        self.body.0.len() as u64
    }

    /// Return the WARC version string of this record.
    pub fn warc_version(&self) -> &str {
        &self.headers.version
    }

    /// Set the WARC version string of this record.
    pub fn set_warc_version<S: Into<String>>(&mut self, id: S) {
        self.headers.version = id.into();
    }

    /// Return the WARC-Record-ID header for this record.
    pub fn warc_id(&self) -> &str {
        &self.record_id
    }

    /// Set the WARC-Record-ID header for this record.
    ///
    /// Note that this value is **not** checked for validity.
    pub fn set_warc_id<S: Into<String>>(&mut self, id: S) {
        self.record_id = id.into();
    }

    /// Return the WARC-Type header for this record.
    pub fn warc_type(&self) -> &RecordType {
        &self.record_type
    }

    /// Set the WARC-Type header for this record.
    pub fn set_warc_type(&mut self, type_: RecordType) {
        self.record_type = type_;
    }

    /// Return the WARC-Date header for this record.
    pub fn date(&self) -> &DateTime<Utc> {
        &self.record_date
    }

    /// Set the WARC-Date header for this record.
    pub fn set_date(&mut self, date: DateTime<Utc>) {
        self.record_date = date;
    }

    /// Return the WARC-Truncated header for this record.
    pub fn truncated_type(&self) -> &Option<TruncatedType> {
        &self.truncated_type
    }

    /// Set the WARC-Truncated header for this record.
    pub fn set_truncated_type(&mut self, truncated_type: TruncatedType) {
        self.truncated_type = Some(truncated_type);
    }

    /// Remove the WARC-Truncated header for this record.
    pub fn clear_truncated_type(&mut self) {
        self.truncated_type = None;
    }

    /// Return the WARC header requested if present in this record, or `None`.
    pub fn header(&self, header: WarcHeader) -> Option<Cow<'_, str>> {
        match &header {
            WarcHeader::ContentLength => Some(Cow::Owned(format!("{}", self.content_length()))),
            WarcHeader::RecordID => Some(Cow::Borrowed(self.warc_id())),
            WarcHeader::WarcType => Some(Cow::Owned(self.record_type.to_string())),
            WarcHeader::Date => Some(Cow::Owned(
                self.date().to_rfc3339_opts(SecondsFormat::Secs, true),
            )),
            _ => self
                .headers
                .as_ref()
                .get(&header)
                .map(|h| Cow::Owned(String::from_utf8(h.clone()).unwrap())),
        }
    }

    /// Set a WARC header in this record, returning the previous value if present.
    ///
    /// # Errors
    ///
    /// If setting a header whose value has a well-formedness test, an error is returned if the
    /// value is not well-formed.
    pub fn set_header<V>(
        &mut self,
        header: WarcHeader,
        value: V,
    ) -> Result<Option<Cow<'_, str>>, WarcError>
    where
        V: Into<String>,
    {
        let value = value.into();
        match &header {
            WarcHeader::Date => {
                let old_date =
                    std::mem::replace(&mut self.record_date, Record::parse_record_date(&value)?);
                Ok(Some(Cow::Owned(
                    old_date.to_rfc3339_opts(SecondsFormat::Secs, true),
                )))
            }
            WarcHeader::RecordID => {
                let old_id = std::mem::replace(&mut self.record_id, value);
                Ok(Some(Cow::Owned(old_id)))
            }
            WarcHeader::WarcType => {
                let old_type = std::mem::replace(&mut self.record_type, RecordType::from(&value));
                Ok(Some(Cow::Owned(old_type.to_string())))
            }
            WarcHeader::Truncated => {
                let old_type = self.truncated_type.take();
                self.truncated_type = Some(TruncatedType::from(&value));
                Ok(old_type.map(|old| (Cow::Owned(old.to_string()))))
            }
            WarcHeader::ContentLength => {
                if Record::parse_content_length(&value)? != self.content_length() {
                    Err(WarcError::MalformedHeader(
                        WarcHeader::ContentLength,
                        "content length != body size".to_string(),
                    ))
                } else {
                    Ok(Some(Cow::Owned(value)))
                }
            }
            _ => Ok(self
                .headers
                .as_mut()
                .insert(header, Vec::from(value))
                .map(|v| Cow::Owned(String::from_utf8(v).unwrap()))),
        }
    }

    /// Return the body of this record.
    pub fn body(&self) -> &[u8] {
        self.body.0.as_slice()
    }

    /// Return a reference to mutate the body of this record, but without changing its length.
    ///
    /// To update the body of the record or change its length, use the `replace_body` method
    /// instead.
    pub fn body_mut(&mut self) -> &mut [u8] {
        self.body.0.as_mut_slice()
    }

    /// Replace the body of this record with the given body.
    pub fn replace_body<V: Into<Vec<u8>>>(&mut self, new_body: V) {
        let _: Vec<u8> = std::mem::replace(&mut self.body.0, new_body.into());
    }
}

impl Default for Record<BufferedBody> {
    fn default() -> Record<BufferedBody> {
        Record {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_string(),
                headers: HashMap::new(),
            },
            record_date: Utc::now(),
            record_id: Record::generate_record_id(),
            record_type: RecordType::Resource,
            truncated_type: None,
            body: BufferedBody(vec![]),
        }
    }
}

impl std::convert::TryFrom<RawRecord> for Record<BufferedBody> {
    type Error = WarcError;
    fn try_from(mut raw: RawRecord) -> Result<Self, WarcError> {
        raw.headers
            .as_mut()
            .remove(&WarcHeader::ContentLength)
            .ok_or_else(|| WarcError::MissingHeader(WarcHeader::ContentLength))
            .and_then(|vec| {
                String::from_utf8(vec).map_err(|_| {
                    WarcError::MalformedHeader(WarcHeader::Date, "not a UTF-8 string".to_string())
                })
            })
            .and_then(|len| Record::parse_content_length(&len))
            .and_then(|len| {
                if len == raw.body.len() as u64 {
                    Ok(())
                } else {
                    Err(WarcError::MalformedHeader(
                        WarcHeader::ContentLength,
                        "content length != body length".to_string(),
                    ))
                }
            })?;

        let record_type = raw
            .headers
            .as_mut()
            .remove(&WarcHeader::WarcType)
            .ok_or_else(|| WarcError::MissingHeader(WarcHeader::WarcType))
            .and_then(|vec| {
                String::from_utf8(vec).map_err(|_| {
                    WarcError::MalformedHeader(
                        WarcHeader::WarcType,
                        "not a UTF-8 string".to_string(),
                    )
                })
            })
            .map(|rtype| rtype.into())?;

        let record_id = raw
            .headers
            .as_mut()
            .remove(&WarcHeader::RecordID)
            .ok_or_else(|| WarcError::MissingHeader(WarcHeader::RecordID))
            .and_then(|vec| {
                String::from_utf8(vec).map_err(|_| {
                    WarcError::MalformedHeader(WarcHeader::Date, "not a UTF-8 string".to_string())
                })
            })?;

        let record_date = raw
            .headers
            .as_mut()
            .remove(&WarcHeader::Date)
            .ok_or_else(|| WarcError::MissingHeader(WarcHeader::Date))
            .and_then(|vec| {
                String::from_utf8(vec).map_err(|_| {
                    WarcError::MalformedHeader(WarcHeader::Date, "not a UTF-8 string".to_string())
                })
            })
            .and_then(|date| Record::parse_record_date(&date))?;

        let RawRecord { headers, body } = raw;
        Ok(Record {
            headers,
            record_date,
            record_id,
            record_type,
            body: BufferedBody(body),
            ..Default::default()
        })
    }
}

impl fmt::Display for Record<BufferedBody> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (headers, body) = self.clone().into_raw_parts();
        write!(f, "Record({}, {:?})", headers, body)
    }
}

impl std::convert::From<Record<BufferedBody>> for RawRecord {
    fn from(record: Record<BufferedBody>) -> RawRecord {
        let (headers, body) = record.clone().into_raw_parts();
        RawRecord { headers, body }
    }
}

impl fmt::Display for RawRecord {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "WARC/{}", self.headers.version)?;

        for (token, value) in self.headers.as_ref().iter() {
            writeln!(
                f,
                "{}: {}",
                token.to_string(),
                String::from_utf8_lossy(value)
            )?;
        }
        writeln!(f)?;

        if !self.body.is_empty() {
            writeln!(f, "\n{}", String::from_utf8_lossy(&self.body))?;
        }

        writeln!(f)?;

        Ok(())
    }
}

impl RecordBuilder {
    pub fn body(&mut self, body: Vec<u8>) -> &mut Self {
        self.value.replace_body(body);

        self
    }

    pub fn date(&mut self, date: DateTime<Utc>) -> &mut Self {
        self.value.set_date(date);

        self
    }

    pub fn warc_id<S: Into<String>>(&mut self, id: S) -> &mut Self {
        self.value.set_warc_id(id);

        self
    }

    pub fn version(&mut self, version: String) -> &mut Self {
        self.value.set_warc_version(version);

        self
    }

    pub fn warc_type(&mut self, warc_type: RecordType) -> &mut Self {
        self.value.set_warc_type(warc_type);

        self
    }

    pub fn truncated_type(&mut self, trunc_type: TruncatedType) -> &mut Self {
        self.value.set_truncated_type(trunc_type);

        self
    }

    pub fn header<V: Into<Vec<u8>>>(&mut self, key: WarcHeader, value: V) -> &mut Self {
        self.broken_headers.insert(key.clone(), value.into());

        let is_ok;
        match std::str::from_utf8(self.broken_headers.get(&key).unwrap()) {
            Ok(string) => {
                if let Err(e) = self.value.set_header(key.clone(), string) {
                    self.last_error = Some(e);
                    is_ok = false;
                } else {
                    is_ok = true;
                }
            }
            Err(_) => {
                is_ok = false;
                self.last_error = Some(WarcError::MalformedHeader(
                    key.clone(),
                    "not a UTF-8 string".to_string(),
                ));
            }
        }

        if is_ok {
            self.broken_headers.remove(&key);
        }

        self
    }

    pub fn build_raw(self) -> (RawHeaderBlock, Vec<u8>) {
        let RecordBuilder {
            value,
            broken_headers,
            ..
        } = self;
        let (mut headers, body) = value.into_raw_parts();
        headers.as_mut().extend(broken_headers);

        (headers, body)
    }

    pub fn build(self) -> Result<Record<BufferedBody>, WarcError> {
        let RecordBuilder {
            value,
            broken_headers,
            last_error,
        } = self;

        if let Some(e) = last_error {
            Err(e)
        } else {
            debug_assert!(
                broken_headers.is_empty(),
                "invariant violation: broken headers without last error"
            );
            Ok(value)
        }
    }
}

#[cfg(test)]
mod record_tests {
    use crate::header::WarcHeader;
    use crate::{Record, RecordType};

    use chrono::prelude::*;

    #[test]
    fn default() {
        let before = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let record = Record::default();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let after = Utc::now();
        assert_eq!(record.content_length(), 0);
        assert_eq!(record.warc_version(), "WARC/1.0");
        assert_eq!(record.warc_type(), &RecordType::Resource);
        assert!(record.date() > &before);
        assert!(record.date() < &after);
    }

    #[test]
    fn impl_eq() {
        let record1 = Record::default();
        let record2 = record1.clone();
        assert_eq!(record1, record2);
    }

    #[test]
    fn body() {
        let mut record = Record::default();
        assert_eq!(record.content_length(), 0);
        assert_eq!(record.body(), &[]);
        record.replace_body(b"hello!!".to_vec());
        assert_eq!(record.content_length(), 7);
        assert_eq!(record.body(), b"hello!!");
        record.body_mut().copy_from_slice(b"goodbye");
        assert_eq!(record.content_length(), 7);
        assert_eq!(record.body(), b"goodbye");
    }

    #[test]
    fn add_header() {
        let mut record = Record::default();
        assert!(record.header(WarcHeader::TargetURI).is_none());
        assert!(record
            .set_header(WarcHeader::TargetURI, "https://www.rust-lang.org")
            .unwrap()
            .is_none());
        assert_eq!(
            record.header(WarcHeader::TargetURI).unwrap(),
            "https://www.rust-lang.org"
        );
        assert_eq!(
            record
                .set_header(WarcHeader::TargetURI, "https://docs.rs")
                .unwrap()
                .unwrap(),
            "https://www.rust-lang.org"
        );
        assert_eq!(
            record.header(WarcHeader::TargetURI).unwrap(),
            "https://docs.rs"
        );
    }

    #[test]
    fn set_header_override_content_length() {
        let mut record = Record::default();
        assert_eq!(record.header(WarcHeader::ContentLength).unwrap(), "0");
        assert!(record
            .set_header(WarcHeader::ContentLength, "really short")
            .is_err());
        assert!(record.set_header(WarcHeader::ContentLength, "50").is_err());
        assert_eq!(
            record
                .set_header(WarcHeader::ContentLength, "0")
                .unwrap()
                .unwrap(),
            "0"
        );
    }

    #[test]
    fn set_header_override_warc_date() {
        let mut record = Record::default();
        let old_date = record.date().to_rfc3339_opts(SecondsFormat::Secs, true);
        assert_eq!(record.header(WarcHeader::Date).unwrap(), old_date);
        assert!(record.set_header(WarcHeader::Date, "yesterday").is_err());
        assert_eq!(
            record
                .set_header(WarcHeader::Date, "2020-07-21T22:00:00Z")
                .unwrap()
                .unwrap(),
            old_date
        );
        assert_eq!(
            record.header(WarcHeader::Date).unwrap(),
            "2020-07-21T22:00:00Z"
        );
    }

    #[test]
    fn set_header_override_warc_record_id() {
        let mut record = Record::default();
        let old_id = record.warc_id().to_string();
        assert_eq!(
            record.header(WarcHeader::RecordID).unwrap(),
            old_id.as_str()
        );
        assert_eq!(
            record
                .set_header(WarcHeader::RecordID, "urn:http:www.rust-lang.org")
                .unwrap()
                .unwrap(),
            old_id.as_str()
        );
        assert_eq!(
            record.header(WarcHeader::RecordID).unwrap(),
            "urn:http:www.rust-lang.org"
        );
    }

    #[test]
    fn set_header_override_warc_type() {
        let mut record = Record::default();
        assert_eq!(record.header(WarcHeader::WarcType).unwrap(), "resource");
        assert_eq!(
            record
                .set_header(WarcHeader::WarcType, "revisit")
                .unwrap()
                .unwrap(),
            "resource"
        );
        assert_eq!(record.header(WarcHeader::WarcType).unwrap(), "revisit");
    }
}

#[cfg(test)]
mod raw_tests {
    use crate::header::WarcHeader;
    use crate::{RawHeaderBlock, RawRecord, Record, RecordType};

    use std::collections::HashMap;
    use std::convert::TryFrom;

    #[test]
    fn create() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: HashMap::new(),
            },
            body: vec![],
        };

        assert_eq!(record.body.len(), 0);
    }

    #[test]
    fn create_with_headers() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![(
                    WarcHeader::WarcType,
                    RecordType::WarcInfo.to_string().into_bytes(),
                )]
                .into_iter()
                .collect(),
            },
            body: vec![],
        };

        assert_eq!(record.headers.as_ref().len(), 1);
    }

    #[test]
    fn verify_ok() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![
                    (WarcHeader::WarcType, b"dunno".to_vec()),
                    (WarcHeader::ContentLength, b"5".to_vec()),
                    (
                        WarcHeader::RecordID,
                        b"<urn:test:basic-record:record-0>".to_vec(),
                    ),
                    (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
                ]
                .into_iter()
                .collect(),
            },
            body: b"12345".to_vec(),
        };

        assert!(Record::try_from(record).is_ok());
    }

    #[test]
    fn verify_missing_type() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![
                    (WarcHeader::ContentLength, b"5".to_vec()),
                    (
                        WarcHeader::RecordID,
                        b"<urn:test:basic-record:record-0>".to_vec(),
                    ),
                    (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
                ]
                .into_iter()
                .collect(),
            },
            body: b"12345".to_vec(),
        };

        assert!(Record::try_from(record).is_err());
    }

    #[test]
    fn verify_missing_content_length() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![
                    (WarcHeader::WarcType, b"dunno".to_vec()),
                    (
                        WarcHeader::RecordID,
                        b"<urn:test:basic-record:record-0>".to_vec(),
                    ),
                    (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
                ]
                .into_iter()
                .collect(),
            },
            body: b"12345".to_vec(),
        };

        assert!(Record::try_from(record).is_err());
    }

    #[test]
    fn verify_missing_record_id() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![
                    (WarcHeader::WarcType, b"dunno".to_vec()),
                    (WarcHeader::ContentLength, b"5".to_vec()),
                    (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
                ]
                .into_iter()
                .collect(),
            },
            body: b"12345".to_vec(),
        };

        assert!(Record::try_from(record).is_err());
    }

    #[test]
    fn verify_missing_date() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![
                    (WarcHeader::WarcType, b"dunno".to_vec()),
                    (WarcHeader::ContentLength, b"5".to_vec()),
                    (
                        WarcHeader::RecordID,
                        b"<urn:test:basic-record:record-0>".to_vec(),
                    ),
                ]
                .into_iter()
                .collect(),
            },
            body: b"12345".to_vec(),
        };

        assert!(Record::try_from(record).is_err());
    }
}

#[cfg(test)]
mod builder_tests {
    use crate::header::WarcHeader;
    use crate::{RawHeaderBlock, RawRecord, Record, RecordBuilder, RecordType, TruncatedType};

    use std::convert::TryFrom;

    #[test]
    fn default() {
        let (headers, body) = RecordBuilder::default().build_raw();
        assert_eq!(headers.version, "WARC/1.0".to_string());
        assert_eq!(
            headers.as_ref().get(&WarcHeader::ContentLength).unwrap(),
            &b"0".to_vec()
        );
        assert!(body.is_empty());
        assert!(RecordBuilder::default().build().is_ok());
    }

    #[test]
    fn impl_eq_raw() {
        let builder = RecordBuilder::default();
        let raw1 = builder.clone().build_raw();

        let raw2 = builder.build_raw();
        assert_eq!(raw1, raw2);
    }

    #[test]
    fn impl_eq_record() {
        let builder = RecordBuilder::default();
        let record1 = builder.clone().build().unwrap();

        let record2 = builder.build().unwrap();
        assert_eq!(record1, record2);
    }

    #[test]
    fn create_with_headers() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![(
                    WarcHeader::WarcType,
                    RecordType::WarcInfo.to_string().into_bytes(),
                )]
                .into_iter()
                .collect(),
            },
            body: vec![],
        };

        assert_eq!(record.headers.as_ref().len(), 1);
    }

    #[test]
    fn verify_ok() {
        let record = RawRecord {
            headers: RawHeaderBlock {
                version: "WARC/1.0".to_owned(),
                headers: vec![
                    (WarcHeader::WarcType, b"dunno".to_vec()),
                    (WarcHeader::ContentLength, b"5".to_vec()),
                    (
                        WarcHeader::RecordID,
                        b"<urn:test:basic-record:record-0>".to_vec(),
                    ),
                    (WarcHeader::Date, b"2020-07-08T02:52:55Z".to_vec()),
                ]
                .into_iter()
                .collect(),
            },
            body: b"12345".to_vec(),
        };

        assert!(Record::try_from(record).is_ok());
    }

    #[test]
    fn verify_content_length() {
        let mut builder = RecordBuilder::default();
        builder.body(b"12345".to_vec());

        assert_eq!(
            builder
                .clone()
                .build()
                .unwrap()
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::ContentLength)
                .unwrap(),
            &b"5".to_vec()
        );

        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::ContentLength)
                .unwrap(),
            &b"5".to_vec()
        );

        builder.header(WarcHeader::ContentLength, "1");
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::ContentLength)
                .unwrap(),
            &b"1".to_vec()
        );

        assert!(builder.build().is_err());
    }

    #[test]
    fn verify_build_record_type() {
        let mut builder1 = RecordBuilder::default();
        let mut builder2 = builder1.clone();

        builder1.header(WarcHeader::WarcType, "request");
        builder2.warc_type(RecordType::Request);

        let record1 = builder1.build().unwrap();
        let record2 = builder2.build().unwrap();

        assert_eq!(record1, record2);
        assert_eq!(
            record1
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::WarcType),
            Some(&b"request".to_vec())
        );
    }

    #[test]
    fn verify_build_date() {
        const DATE_STRING_0: &str = "2020-07-08T02:52:55Z";
        const DATE_STRING_1: &[u8] = b"2020-07-18T02:12:45Z";

        let mut builder = RecordBuilder::default();
        builder.date(Record::parse_record_date(DATE_STRING_0).unwrap());

        let record = builder.clone().build().unwrap();
        assert_eq!(
            record
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::Date)
                .unwrap(),
            &DATE_STRING_0.as_bytes()
        );
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::Date)
                .unwrap(),
            &DATE_STRING_0.as_bytes()
        );

        builder.header(WarcHeader::Date, DATE_STRING_1.to_vec());
        let record = builder.clone().build().unwrap();
        assert_eq!(
            record
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::Date)
                .unwrap(),
            &DATE_STRING_1.to_vec()
        );
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::Date)
                .unwrap(),
            &DATE_STRING_1.to_vec()
        );

        builder.header(WarcHeader::Date, b"not-a-dayTor:a:time".to_vec());
        assert!(builder.build().is_err());
    }

    #[test]
    fn verify_build_record_id() {
        const RECORD_ID_0: &[u8] = b"<urn:test:verify-build-id:record-0>";
        const RECORD_ID_1: &[u8] = b"<urn:test:verify-build-id:record-1>";

        let mut builder = RecordBuilder::default();
        builder.warc_id(std::str::from_utf8(RECORD_ID_0).unwrap());

        let record = builder.clone().build().unwrap();
        assert_eq!(
            record
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::RecordID)
                .unwrap(),
            &RECORD_ID_0.to_vec()
        );
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::RecordID)
                .unwrap(),
            &RECORD_ID_0.to_vec()
        );

        builder.header(WarcHeader::RecordID, RECORD_ID_1.to_vec());
        let record = builder.clone().build().unwrap();
        assert_eq!(
            record
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::RecordID)
                .unwrap(),
            &RECORD_ID_1.to_vec()
        );
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::RecordID)
                .unwrap(),
            &RECORD_ID_1.to_vec()
        );
    }

    #[test]
    fn verify_build_truncated_type() {
        const TRUNCATED_TYPE_0: &[u8] = b"length";
        const TRUNCATED_TYPE_1: &[u8] = b"disconnect";

        let mut builder = RecordBuilder::default();
        builder.truncated_type(TruncatedType::Length);

        let record = builder.clone().build().unwrap();
        assert_eq!(
            record
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::Truncated)
                .unwrap(),
            &TRUNCATED_TYPE_0.to_vec()
        );
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::Truncated)
                .unwrap(),
            &TRUNCATED_TYPE_0.to_vec()
        );

        builder.header(WarcHeader::Truncated, "disconnect");
        let record = builder.clone().build().unwrap();
        assert_eq!(
            record
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::Truncated)
                .unwrap(),
            &TRUNCATED_TYPE_1.to_vec()
        );
        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::Truncated)
                .unwrap(),
            &TRUNCATED_TYPE_1.to_vec()
        );

        builder.header(WarcHeader::Truncated, "foreign-intervention");
        assert_eq!(
            builder
                .clone()
                .build()
                .unwrap()
                .into_raw_parts()
                .0
                .as_ref()
                .get(&WarcHeader::Truncated)
                .unwrap()
                .as_slice(),
            &b"foreign-intervention"[..]
        );

        assert_eq!(
            builder
                .clone()
                .build_raw()
                .0
                .as_ref()
                .get(&WarcHeader::Truncated)
                .unwrap()
                .as_slice(),
            &b"foreign-intervention"[..]
        );
    }
}
