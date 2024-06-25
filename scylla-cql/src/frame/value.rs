use crate::frame::frame_errors::ParseError;
use crate::frame::types;
use bytes::BufMut;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryInto;
use std::hash::BuildHasher;
use std::net::IpAddr;
use thiserror::Error;
use uuid::Uuid;

use super::response::result::CqlValue;
use super::types::vint_encode;
use super::types::RawValue;

/// Every value being sent in a query must implement this trait
/// serialize() should write the Value as [bytes] to the provided buffer
pub trait Value {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig>;
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[error("Value too big to be sent in a request - max 2GiB allowed")]
pub struct ValueTooBig;

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[error("Value is too large to fit in the CQL type")]
pub struct ValueOverflow;

/// Represents an unset value
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unset;

/// Represents an counter value
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Counter(pub i64);

/// Enum providing a way to represent a value that might be unset
#[derive(Clone, Copy, Default)]
pub enum MaybeUnset<V> {
    #[default]
    Unset,
    Set(V),
}

/// Represents timeuuid (uuid V1) value
///
/// This type has custom comparison logic which follows Scylla/Cassandra semantics.
/// For details, see [`Ord` implementation](#impl-Ord-for-CqlTimeuuid).
#[derive(Debug, Clone, Copy, Eq)]
pub struct CqlTimeuuid(Uuid);

/// [`Uuid`] delegate methods
impl CqlTimeuuid {
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    pub fn as_u128(&self) -> u128 {
        self.0.as_u128()
    }

    pub fn as_fields(&self) -> (u32, u16, u16, &[u8; 8]) {
        self.0.as_fields()
    }

    pub fn as_u64_pair(&self) -> (u64, u64) {
        self.0.as_u64_pair()
    }

    pub fn from_slice(b: &[u8]) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::from_slice(b)?))
    }

    pub fn from_slice_le(b: &[u8]) -> Result<Self, uuid::Error> {
        Ok(Self(Uuid::from_slice_le(b)?))
    }

    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }

    pub fn from_bytes_le(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes_le(bytes))
    }

    pub fn from_fields(d1: u32, d2: u16, d3: u16, d4: &[u8; 8]) -> Self {
        Self(Uuid::from_fields(d1, d2, d3, d4))
    }

    pub fn from_fields_le(d1: u32, d2: u16, d3: u16, d4: &[u8; 8]) -> Self {
        Self(Uuid::from_fields_le(d1, d2, d3, d4))
    }

    pub fn from_u128(v: u128) -> Self {
        Self(Uuid::from_u128(v))
    }

    pub fn from_u128_le(v: u128) -> Self {
        Self(Uuid::from_u128_le(v))
    }

    pub fn from_u64_pair(high_bits: u64, low_bits: u64) -> Self {
        Self(Uuid::from_u64_pair(high_bits, low_bits))
    }
}

impl CqlTimeuuid {
    /// Read 8 most significant bytes of timeuuid from serialized bytes
    fn msb(&self) -> u64 {
        // Scylla and Cassandra use a standard UUID memory layout for MSB:
        // 4 bytes    2 bytes    2 bytes
        // time_low - time_mid - time_hi_and_version
        let bytes = self.0.as_bytes();
        ((bytes[6] & 0x0F) as u64) << 56
            | (bytes[7] as u64) << 48
            | (bytes[4] as u64) << 40
            | (bytes[5] as u64) << 32
            | (bytes[0] as u64) << 24
            | (bytes[1] as u64) << 16
            | (bytes[2] as u64) << 8
            | (bytes[3] as u64)
    }

    fn lsb(&self) -> u64 {
        let bytes = self.0.as_bytes();
        (bytes[8] as u64) << 56
            | (bytes[9] as u64) << 48
            | (bytes[10] as u64) << 40
            | (bytes[11] as u64) << 32
            | (bytes[12] as u64) << 24
            | (bytes[13] as u64) << 16
            | (bytes[14] as u64) << 8
            | (bytes[15] as u64)
    }

    fn lsb_signed(&self) -> u64 {
        self.lsb() ^ 0x8080808080808080
    }
}

impl std::str::FromStr for CqlTimeuuid {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::from_str(s)?))
    }
}

impl std::fmt::Display for CqlTimeuuid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<Uuid> for CqlTimeuuid {
    fn as_ref(&self) -> &Uuid {
        &self.0
    }
}

impl From<CqlTimeuuid> for Uuid {
    fn from(value: CqlTimeuuid) -> Self {
        value.0
    }
}

impl From<Uuid> for CqlTimeuuid {
    fn from(value: Uuid) -> Self {
        Self(value)
    }
}

/// Compare two values of timeuuid type.
///
/// Cassandra legacy requires:
/// - converting 8 most significant bytes to date, which is then compared.
/// - masking off UUID version from the 8 ms-bytes during compare, to
///   treat possible non-version-1 UUID the same way as UUID.
/// - using signed compare for least significant bits.
impl Ord for CqlTimeuuid {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let mut res = self.msb().cmp(&other.msb());
        if let std::cmp::Ordering::Equal = res {
            res = self.lsb_signed().cmp(&other.lsb_signed());
        }
        res
    }
}

impl PartialOrd for CqlTimeuuid {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for CqlTimeuuid {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl std::hash::Hash for CqlTimeuuid {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.lsb_signed().hash(state);
        self.msb().hash(state);
    }
}

/// Native CQL `varint` representation.
///
/// Represented as two's-complement binary in big-endian order.
///
/// This type is a raw representation in bytes. It's the default
/// implementation of `varint` type - independent of any
/// external crates and crate features.
///
/// The type is not very useful in most use cases.
/// However, users can make use of more complex types
/// such as `num_bigint::BigInt` (v0.3/v0.4).
/// The library support (e.g. conversion from [`CqlValue`]) for these types is
/// enabled via `num-bigint-03` and `num-bigint-04` crate features.
///
/// # DB data format
/// Notice that [constructors](CqlVarint#impl-CqlVarint)
/// don't perform any normalization on the provided data.
/// This means that underlying bytes may contain leading zeros.
///
/// Currently, Scylla and Cassandra support non-normalized `varint` values.
/// Bytes provided by the user via constructor are passed to DB as is.
///
/// The implementation of [`PartialEq`], however, normalizes the underlying bytes
/// before comparison. For details, check [examples](#impl-PartialEq-for-CqlVarint).
#[derive(Clone, Eq, Debug)]
pub struct CqlVarint(Vec<u8>);

/// Constructors from bytes
impl CqlVarint {
    /// Creates a [`CqlVarint`] from an array of bytes in
    /// two's complement big-endian binary representation.
    ///
    /// See: disclaimer about [non-normalized values](CqlVarint#db-data-format).
    pub fn from_signed_bytes_be(digits: Vec<u8>) -> Self {
        Self(digits)
    }

    /// Creates a [`CqlVarint`] from a slice of bytes in
    /// two's complement binary big-endian representation.
    ///
    /// See: disclaimer about [non-normalized values](CqlVarint#db-data-format).
    pub fn from_signed_bytes_be_slice(digits: &[u8]) -> Self {
        Self::from_signed_bytes_be(digits.to_vec())
    }
}

/// Conversion to bytes
impl CqlVarint {
    /// Converts [`CqlVarint`] to an array of bytes in two's
    /// complement binary big-endian representation.
    pub fn into_signed_bytes_be(self) -> Vec<u8> {
        self.0
    }

    /// Returns a slice of bytes in two's complement
    /// binary big-endian representation.
    pub fn as_signed_bytes_be_slice(&self) -> &[u8] {
        &self.0
    }
}

impl CqlVarint {
    fn as_normalized_slice(&self) -> &[u8] {
        let digits = self.0.as_slice();
        if digits.is_empty() {
            // num-bigint crate normalizes empty vector to 0.
            // We will follow the same approach.
            return &[0];
        }

        let non_zero_position = match digits.iter().position(|b| *b != 0) {
            Some(pos) => pos,
            None => {
                // Vector is filled with zeros. Represent it as 0.
                return &[0];
            }
        };

        if non_zero_position > 0 {
            // There were some leading zeros.
            // Now, there are two cases:
            let zeros_to_remove = if digits[non_zero_position] > 0x7f {
                // Most significant bit is 1, so we need to include one of the leading
                // zeros as originally it represented a positive number.
                non_zero_position - 1
            } else {
                // Most significant bit is 0 - positive number with no leading zeros.
                non_zero_position
            };
            return &digits[zeros_to_remove..];
        }

        // There were no leading zeros at all - leave as is.
        digits
    }
}

#[cfg(feature = "num-bigint-03")]
impl From<num_bigint_03::BigInt> for CqlVarint {
    fn from(value: num_bigint_03::BigInt) -> Self {
        Self(value.to_signed_bytes_be())
    }
}

#[cfg(feature = "num-bigint-03")]
impl From<CqlVarint> for num_bigint_03::BigInt {
    fn from(val: CqlVarint) -> Self {
        num_bigint_03::BigInt::from_signed_bytes_be(&val.0)
    }
}

#[cfg(feature = "num-bigint-04")]
impl From<num_bigint_04::BigInt> for CqlVarint {
    fn from(value: num_bigint_04::BigInt) -> Self {
        Self(value.to_signed_bytes_be())
    }
}

#[cfg(feature = "num-bigint-04")]
impl From<CqlVarint> for num_bigint_04::BigInt {
    fn from(val: CqlVarint) -> Self {
        num_bigint_04::BigInt::from_signed_bytes_be(&val.0)
    }
}

/// Compares two [`CqlVarint`] values after normalization.
///
/// # Example
///
/// ```rust
/// # use scylla_cql::frame::value::CqlVarint;
/// let non_normalized_bytes = vec![0x00, 0x01];
/// let normalized_bytes = vec![0x01];
/// assert_eq!(
///     CqlVarint::from_signed_bytes_be(non_normalized_bytes),
///     CqlVarint::from_signed_bytes_be(normalized_bytes)
/// );
/// ```
impl PartialEq for CqlVarint {
    fn eq(&self, other: &Self) -> bool {
        self.as_normalized_slice() == other.as_normalized_slice()
    }
}

/// Computes the hash of normalized [`CqlVarint`].
impl std::hash::Hash for CqlVarint {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_normalized_slice().hash(state)
    }
}

/// Native CQL `decimal` representation.
///
/// Represented as a pair:
/// - a [`CqlVarint`] value
/// - 32-bit integer which determines the position of the decimal point
///
/// The type is not very useful in most use cases.
/// However, users can make use of more complex types
/// such as `bigdecimal::BigDecimal` (v0.4).
/// The library support (e.g. conversion from [`CqlValue`]) for the type is
/// enabled via `bigdecimal-04` crate feature.
///
/// # DB data format
/// Notice that [constructors](CqlDecimal#impl-CqlDecimal)
/// don't perform any normalization on the provided data.
/// For more details, see [`CqlVarint`] documentation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CqlDecimal {
    int_val: CqlVarint,
    scale: i32,
}

/// Constructors
impl CqlDecimal {
    /// Creates a [`CqlDecimal`] from an array of bytes
    /// representing [`CqlVarint`] and a 32-bit scale.
    ///
    /// See: disclaimer about [non-normalized values](CqlVarint#db-data-format).
    pub fn from_signed_be_bytes_and_exponent(bytes: Vec<u8>, scale: i32) -> Self {
        Self {
            int_val: CqlVarint::from_signed_bytes_be(bytes),
            scale,
        }
    }

    /// Creates a [`CqlDecimal`] from a slice of bytes
    /// representing [`CqlVarint`] and a 32-bit scale.
    ///
    /// See: disclaimer about [non-normalized values](CqlVarint#db-data-format).
    pub fn from_signed_be_bytes_slice_and_exponent(bytes: &[u8], scale: i32) -> Self {
        Self::from_signed_be_bytes_and_exponent(bytes.to_vec(), scale)
    }
}

/// Conversion to raw bytes
impl CqlDecimal {
    /// Returns a slice of bytes in two's complement
    /// binary big-endian representation and a scale.
    pub fn as_signed_be_bytes_slice_and_exponent(&self) -> (&[u8], i32) {
        (self.int_val.as_signed_bytes_be_slice(), self.scale)
    }

    /// Converts [`CqlDecimal`] to an array of bytes in two's
    /// complement binary big-endian representation and a scale.
    pub fn into_signed_be_bytes_and_exponent(self) -> (Vec<u8>, i32) {
        (self.int_val.into_signed_bytes_be(), self.scale)
    }
}

#[cfg(feature = "bigdecimal-04")]
impl From<CqlDecimal> for bigdecimal_04::BigDecimal {
    fn from(value: CqlDecimal) -> Self {
        Self::from((
            bigdecimal_04::num_bigint::BigInt::from_signed_bytes_be(
                value.int_val.as_signed_bytes_be_slice(),
            ),
            value.scale as i64,
        ))
    }
}

#[cfg(feature = "bigdecimal-04")]
impl TryFrom<bigdecimal_04::BigDecimal> for CqlDecimal {
    type Error = <i64 as TryInto<i32>>::Error;

    fn try_from(value: bigdecimal_04::BigDecimal) -> Result<Self, Self::Error> {
        let (bigint, scale) = value.into_bigint_and_exponent();
        let bytes = bigint.to_signed_bytes_be();
        Ok(Self::from_signed_be_bytes_and_exponent(
            bytes,
            scale.try_into()?,
        ))
    }
}

/// Native CQL date representation that allows for a bigger range of dates (-262145-1-1 to 262143-12-31).
///
/// Represented as number of days since -5877641-06-23 i.e. 2^31 days before unix epoch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CqlDate(pub u32);

/// Native CQL timestamp representation that allows full supported timestamp range.
///
/// Represented as signed milliseconds since unix epoch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CqlTimestamp(pub i64);

/// Native CQL time representation.
///
/// Represented as nanoseconds since midnight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CqlTime(pub i64);

#[cfg(feature = "chrono-04")]
impl From<chrono_04::NaiveDate> for CqlDate {
    fn from(value: chrono_04::NaiveDate) -> Self {
        let unix_epoch = chrono_04::NaiveDate::from_yo_opt(1970, 1).unwrap();

        // `NaiveDate` range is -262145-01-01 to 262143-12-31
        // Both values are well within supported range
        let days = ((1 << 31) + value.signed_duration_since(unix_epoch).num_days()) as u32;

        Self(days)
    }
}

#[cfg(feature = "chrono-04")]
impl TryInto<chrono_04::NaiveDate> for CqlDate {
    type Error = ValueOverflow;

    fn try_into(self) -> Result<chrono_04::NaiveDate, Self::Error> {
        let days_since_unix_epoch = self.0 as i64 - (1 << 31);

        // date_days is u32 then converted to i64 then we subtract 2^31;
        // Max value is 2^31, min value is -2^31. Both values can safely fit in chrono::Duration, this call won't panic
        let duration_since_unix_epoch =
            chrono_04::Duration::try_days(days_since_unix_epoch).unwrap();

        chrono_04::NaiveDate::from_yo_opt(1970, 1)
            .unwrap()
            .checked_add_signed(duration_since_unix_epoch)
            .ok_or(ValueOverflow)
    }
}

#[cfg(feature = "chrono-04")]
impl From<chrono_04::DateTime<chrono_04::Utc>> for CqlTimestamp {
    fn from(value: chrono_04::DateTime<chrono_04::Utc>) -> Self {
        Self(value.timestamp_millis())
    }
}

#[cfg(feature = "chrono-04")]
impl TryInto<chrono_04::DateTime<chrono_04::Utc>> for CqlTimestamp {
    type Error = ValueOverflow;

    fn try_into(self) -> Result<chrono_04::DateTime<chrono_04::Utc>, Self::Error> {
        use chrono_04::TimeZone;
        match chrono_04::Utc.timestamp_millis_opt(self.0) {
            chrono_04::LocalResult::Single(datetime) => Ok(datetime),
            _ => Err(ValueOverflow),
        }
    }
}

#[cfg(feature = "chrono-04")]
impl TryFrom<chrono_04::NaiveTime> for CqlTime {
    type Error = ValueOverflow;

    fn try_from(value: chrono_04::NaiveTime) -> Result<Self, Self::Error> {
        let nanos = value
            .signed_duration_since(chrono_04::NaiveTime::MIN)
            .num_nanoseconds()
            .unwrap();

        // Value can exceed max CQL time in case of leap second
        if nanos <= 86399999999999 {
            Ok(Self(nanos))
        } else {
            Err(ValueOverflow)
        }
    }
}

#[cfg(feature = "chrono-04")]
impl TryInto<chrono_04::NaiveTime> for CqlTime {
    type Error = ValueOverflow;

    fn try_into(self) -> Result<chrono_04::NaiveTime, Self::Error> {
        let secs = (self.0 / 1_000_000_000)
            .try_into()
            .map_err(|_| ValueOverflow)?;
        let nanos = (self.0 % 1_000_000_000)
            .try_into()
            .map_err(|_| ValueOverflow)?;
        chrono_04::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos).ok_or(ValueOverflow)
    }
}

#[cfg(feature = "time-03")]
impl From<time_03::Date> for CqlDate {
    fn from(value: time_03::Date) -> Self {
        const JULIAN_DAY_OFFSET: i64 =
            (1 << 31) - time_03::OffsetDateTime::UNIX_EPOCH.date().to_julian_day() as i64;

        // Statically assert that no possible value will ever overflow
        const _: () = assert!(
            time_03::Date::MAX.to_julian_day() as i64 + JULIAN_DAY_OFFSET < u32::MAX as i64
        );
        const _: () = assert!(
            time_03::Date::MIN.to_julian_day() as i64 + JULIAN_DAY_OFFSET > u32::MIN as i64
        );

        let days = value.to_julian_day() as i64 + JULIAN_DAY_OFFSET;

        Self(days as u32)
    }
}

#[cfg(feature = "time-03")]
impl TryInto<time_03::Date> for CqlDate {
    type Error = ValueOverflow;

    fn try_into(self) -> Result<time_03::Date, Self::Error> {
        const JULIAN_DAY_OFFSET: i64 =
            (1 << 31) - time_03::OffsetDateTime::UNIX_EPOCH.date().to_julian_day() as i64;

        let julian_days = (self.0 as i64 - JULIAN_DAY_OFFSET)
            .try_into()
            .map_err(|_| ValueOverflow)?;

        time_03::Date::from_julian_day(julian_days).map_err(|_| ValueOverflow)
    }
}

#[cfg(feature = "time-03")]
impl From<time_03::OffsetDateTime> for CqlTimestamp {
    fn from(value: time_03::OffsetDateTime) -> Self {
        // Statically assert that no possible value will ever overflow. OffsetDateTime doesn't allow offset to overflow
        // the UTC PrimitiveDateTime value value
        const _: () = assert!(
            time_03::PrimitiveDateTime::MAX
                .assume_utc()
                .unix_timestamp_nanos()
                // Nanos to millis
                / 1_000_000
                < i64::MAX as i128
        );
        const _: () = assert!(
            time_03::PrimitiveDateTime::MIN
                .assume_utc()
                .unix_timestamp_nanos()
                / 1_000_000
                > i64::MIN as i128
        );

        // Edge cases were statically asserted above, checked math is not required
        Self(value.unix_timestamp() * 1000 + value.millisecond() as i64)
    }
}

#[cfg(feature = "time-03")]
impl TryInto<time_03::OffsetDateTime> for CqlTimestamp {
    type Error = ValueOverflow;

    fn try_into(self) -> Result<time_03::OffsetDateTime, Self::Error> {
        time_03::OffsetDateTime::from_unix_timestamp_nanos(self.0 as i128 * 1_000_000)
            .map_err(|_| ValueOverflow)
    }
}

#[cfg(feature = "time-03")]
impl From<time_03::Time> for CqlTime {
    fn from(value: time_03::Time) -> Self {
        let (h, m, s, n) = value.as_hms_nano();

        // no need for checked arithmetic as all these types are guaranteed to fit in i64 without overflow
        let nanos = (h as i64 * 3600 + m as i64 * 60 + s as i64) * 1_000_000_000 + n as i64;

        Self(nanos)
    }
}

#[cfg(feature = "time-03")]
impl TryInto<time_03::Time> for CqlTime {
    type Error = ValueOverflow;

    fn try_into(self) -> Result<time_03::Time, Self::Error> {
        let h = self.0 / 3_600_000_000_000;
        let m = self.0 / 60_000_000_000 % 60;
        let s = self.0 / 1_000_000_000 % 60;
        let n = self.0 % 1_000_000_000;

        time_03::Time::from_hms_nano(
            h.try_into().map_err(|_| ValueOverflow)?,
            m as u8,
            s as u8,
            n as u32,
        )
        .map_err(|_| ValueOverflow)
    }
}

/// Keeps a buffer with serialized Values
/// Allows adding new Values and iterating over serialized ones
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LegacySerializedValues {
    serialized_values: Vec<u8>,
    values_num: u16,
    contains_names: bool,
}

/// Represents a CQL Duration value
#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub struct CqlDuration {
    pub months: i32,
    pub days: i32,
    pub nanoseconds: i64,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SerializeValuesError {
    #[error("Too many values to add, max 65,535 values can be sent in a request")]
    TooManyValues,
    #[error("Mixing named and not named values is not allowed")]
    MixingNamedAndNotNamedValues,
    #[error(transparent)]
    ValueTooBig(#[from] ValueTooBig),
    #[error("Parsing serialized values failed")]
    ParseError,
}

pub type SerializedResult<'a> = Result<Cow<'a, LegacySerializedValues>, SerializeValuesError>;

/// Represents list of values to be sent in a query
/// gets serialized and but into request
pub trait ValueList {
    /// Provides a view of ValueList as LegacySerializedValues
    /// returns `Cow<LegacySerializedValues>` to make impl ValueList for LegacySerializedValues efficient
    fn serialized(&self) -> SerializedResult<'_>;

    fn write_to_request(&self, buf: &mut impl BufMut) -> Result<(), SerializeValuesError> {
        let serialized = self.serialized()?;
        LegacySerializedValues::write_to_request(&serialized, buf);

        Ok(())
    }
}

impl Default for LegacySerializedValues {
    fn default() -> Self {
        Self::new()
    }
}

impl LegacySerializedValues {
    /// Creates empty value list
    pub const fn new() -> Self {
        LegacySerializedValues {
            serialized_values: Vec::new(),
            values_num: 0,
            contains_names: false,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        LegacySerializedValues {
            serialized_values: Vec::with_capacity(capacity),
            values_num: 0,
            contains_names: false,
        }
    }

    pub fn has_names(&self) -> bool {
        self.contains_names
    }

    /// A const empty instance, useful for taking references
    pub const EMPTY: &'static LegacySerializedValues = &LegacySerializedValues::new();

    /// Serializes value and appends it to the list
    pub fn add_value(&mut self, val: &impl Value) -> Result<(), SerializeValuesError> {
        if self.contains_names {
            return Err(SerializeValuesError::MixingNamedAndNotNamedValues);
        }
        if self.values_num == u16::MAX {
            return Err(SerializeValuesError::TooManyValues);
        }

        let len_before_serialize: usize = self.serialized_values.len();

        if let Err(e) = val.serialize(&mut self.serialized_values) {
            self.serialized_values.resize(len_before_serialize, 0);
            return Err(SerializeValuesError::from(e));
        }

        self.values_num += 1;
        Ok(())
    }

    pub fn add_named_value(
        &mut self,
        name: &str,
        val: &impl Value,
    ) -> Result<(), SerializeValuesError> {
        if self.values_num > 0 && !self.contains_names {
            return Err(SerializeValuesError::MixingNamedAndNotNamedValues);
        }
        self.contains_names = true;
        if self.values_num == u16::MAX {
            return Err(SerializeValuesError::TooManyValues);
        }

        let len_before_serialize: usize = self.serialized_values.len();

        types::write_string(name, &mut self.serialized_values)
            .map_err(|_| SerializeValuesError::ParseError)?;

        if let Err(e) = val.serialize(&mut self.serialized_values) {
            self.serialized_values.resize(len_before_serialize, 0);
            return Err(SerializeValuesError::from(e));
        }

        self.values_num += 1;
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = RawValue> {
        LegacySerializedValuesIterator {
            serialized_values: &self.serialized_values,
            contains_names: self.contains_names,
        }
    }

    pub fn write_to_request(&self, buf: &mut impl BufMut) {
        buf.put_u16(self.values_num);
        buf.put(&self.serialized_values[..]);
    }

    pub fn is_empty(&self) -> bool {
        self.values_num == 0
    }

    pub fn len(&self) -> u16 {
        self.values_num
    }

    pub fn size(&self) -> usize {
        self.serialized_values.len()
    }

    /// Creates value list from the request frame
    pub fn new_from_frame(buf: &mut &[u8], contains_names: bool) -> Result<Self, ParseError> {
        let values_num = types::read_short(buf)?;
        let values_beg = *buf;
        for _ in 0..values_num {
            if contains_names {
                let _name = types::read_string(buf)?;
            }
            let _serialized = types::read_bytes_opt(buf)?;
        }

        let values_len_in_buf = values_beg.len() - buf.len();
        let values_in_frame = &values_beg[0..values_len_in_buf];
        Ok(LegacySerializedValues {
            serialized_values: values_in_frame.to_vec(),
            values_num,
            contains_names,
        })
    }

    pub fn iter_name_value_pairs(&self) -> impl Iterator<Item = (Option<&str>, RawValue)> {
        let mut buf = &self.serialized_values[..];
        (0..self.values_num).map(move |_| {
            // `unwrap()`s here are safe, as we assume type-safety: if `LegacySerializedValues` exits,
            // we have a guarantee that the layout of the serialized values is valid.
            let name = self
                .contains_names
                .then(|| types::read_string(&mut buf).unwrap());
            let serialized = types::read_value(&mut buf).unwrap();
            (name, serialized)
        })
    }
}

#[derive(Clone, Copy)]
pub struct LegacySerializedValuesIterator<'a> {
    serialized_values: &'a [u8],
    contains_names: bool,
}

impl<'a> Iterator for LegacySerializedValuesIterator<'a> {
    type Item = RawValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.serialized_values.is_empty() {
            return None;
        }

        // In case of named values, skip names
        if self.contains_names {
            types::read_short_bytes(&mut self.serialized_values).expect("badly encoded value name");
        }

        Some(types::read_value(&mut self.serialized_values).expect("badly encoded value"))
    }
}

/// Represents List of ValueList for Batch statement
pub trait LegacyBatchValues {
    /// For some unknown reason, this type, when not resolved to a concrete type for a given async function,
    /// cannot live across await boundaries while maintaining the corresponding future `Send`, unless `'r: 'static`
    ///
    /// See <https://github.com/scylladb/scylla-rust-driver/issues/599> for more details
    type LegacyBatchValuesIter<'r>: LegacyBatchValuesIterator<'r>
    where
        Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_>;
}

/// An iterator-like for `ValueList`
///
/// An instance of this can be easily obtained from `IT: Iterator<Item: ValueList>`: that would be
/// `BatchValuesIteratorFromIterator<IT>`
///
/// It's just essentially making methods from `ValueList` accessible instead of being an actual iterator because of
/// compiler limitations that would otherwise be very complex to overcome.
/// (specifically, types being different would require yielding enums for tuple impls)
pub trait LegacyBatchValuesIterator<'a> {
    fn next_serialized(&mut self) -> Option<SerializedResult<'a>>;
    fn write_next_to_request(
        &mut self,
        buf: &mut impl BufMut,
    ) -> Option<Result<(), SerializeValuesError>>;
    fn skip_next(&mut self) -> Option<()>;
    fn count(mut self) -> usize
    where
        Self: Sized,
    {
        let mut count = 0;
        while self.skip_next().is_some() {
            count += 1;
        }
        count
    }
}

/// Implements `BatchValuesIterator` from an `Iterator` over references to things that implement `ValueList`
///
/// Essentially used internally by this lib to provide implementers of `BatchValuesIterator` for cases
/// that always serialize the same concrete `ValueList` type
pub struct LegacyBatchValuesIteratorFromIterator<IT: Iterator> {
    it: IT,
}

impl<'r, 'a: 'r, IT, VL> LegacyBatchValuesIterator<'r> for LegacyBatchValuesIteratorFromIterator<IT>
where
    IT: Iterator<Item = &'a VL>,
    VL: ValueList + 'a,
{
    fn next_serialized(&mut self) -> Option<SerializedResult<'r>> {
        self.it.next().map(|vl| vl.serialized())
    }
    fn write_next_to_request(
        &mut self,
        buf: &mut impl BufMut,
    ) -> Option<Result<(), SerializeValuesError>> {
        self.it.next().map(|vl| vl.write_to_request(buf))
    }
    fn skip_next(&mut self) -> Option<()> {
        self.it.next().map(|_| ())
    }
}

impl<IT> From<IT> for LegacyBatchValuesIteratorFromIterator<IT>
where
    IT: Iterator,
    IT::Item: ValueList,
{
    fn from(it: IT) -> Self {
        LegacyBatchValuesIteratorFromIterator { it }
    }
}

//
//  Value impls
//

// Implement Value for primitive types
impl Value for i8 {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(1);
        buf.put_i8(*self);
        Ok(())
    }
}

impl Value for i16 {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(2);
        buf.put_i16(*self);
        Ok(())
    }
}

impl Value for i32 {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(4);
        buf.put_i32(*self);
        Ok(())
    }
}

impl Value for i64 {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(8);
        buf.put_i64(*self);
        Ok(())
    }
}

impl Value for CqlDecimal {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let (bytes, scale) = self.as_signed_be_bytes_slice_and_exponent();

        if bytes.len() > (i32::MAX - 4) as usize {
            return Err(ValueTooBig);
        }
        let serialized_len: i32 = bytes.len() as i32 + 4;

        buf.put_i32(serialized_len);
        buf.put_i32(scale);
        buf.extend_from_slice(bytes);

        Ok(())
    }
}

#[cfg(feature = "bigdecimal-04")]
impl Value for bigdecimal_04::BigDecimal {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let (value, scale) = self.as_bigint_and_exponent();

        let serialized = value.to_signed_bytes_be();

        if serialized.len() > (i32::MAX - 4) as usize {
            return Err(ValueTooBig);
        }
        let serialized_len: i32 = serialized.len() as i32 + 4;

        buf.put_i32(serialized_len);
        buf.put_i32(scale.try_into().map_err(|_| ValueTooBig)?);
        buf.extend_from_slice(&serialized);

        Ok(())
    }
}

#[cfg(feature = "chrono-04")]
impl Value for chrono_04::NaiveDate {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        CqlDate::from(*self).serialize(buf)
    }
}

impl Value for CqlDate {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(4);
        buf.put_u32(self.0);
        Ok(())
    }
}

#[cfg(feature = "time-03")]
impl Value for time_03::Date {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        CqlDate::from(*self).serialize(buf)
    }
}

impl Value for CqlTimestamp {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(8);
        buf.put_i64(self.0);
        Ok(())
    }
}

impl Value for CqlTime {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(8);
        buf.put_i64(self.0);
        Ok(())
    }
}

#[cfg(feature = "chrono-04")]
impl Value for chrono_04::DateTime<chrono_04::Utc> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        CqlTimestamp::from(*self).serialize(buf)
    }
}

#[cfg(feature = "time-03")]
impl Value for time_03::OffsetDateTime {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        CqlTimestamp::from(*self).serialize(buf)
    }
}

#[cfg(feature = "chrono-04")]
impl Value for chrono_04::NaiveTime {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        CqlTime::try_from(*self)
            .map_err(|_| ValueTooBig)?
            .serialize(buf)
    }
}

#[cfg(feature = "time-03")]
impl Value for time_03::Time {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        CqlTime::from(*self).serialize(buf)
    }
}

#[cfg(feature = "secrecy-08")]
impl<V: Value + secrecy_08::Zeroize> Value for secrecy_08::Secret<V> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        use secrecy_08::ExposeSecret;
        self.expose_secret().serialize(buf)
    }
}

impl Value for bool {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(1);
        let false_bytes: &[u8] = &[0x00];
        let true_bytes: &[u8] = &[0x01];
        if *self {
            buf.put(true_bytes);
        } else {
            buf.put(false_bytes);
        }

        Ok(())
    }
}

impl Value for f32 {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(4);
        buf.put_f32(*self);
        Ok(())
    }
}

impl Value for f64 {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(8);
        buf.put_f64(*self);
        Ok(())
    }
}

impl Value for Uuid {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        buf.put_i32(16);
        buf.extend_from_slice(self.as_bytes());
        Ok(())
    }
}

impl Value for CqlTimeuuid {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        self.0.serialize(buf)
    }
}

impl Value for CqlVarint {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let serialized = &self.0;
        let serialized_len: i32 = serialized.len().try_into().map_err(|_| ValueTooBig)?;

        buf.put_i32(serialized_len);
        buf.extend_from_slice(serialized);

        Ok(())
    }
}

#[cfg(feature = "num-bigint-03")]
impl Value for num_bigint_03::BigInt {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let serialized = self.to_signed_bytes_be();
        let serialized_len: i32 = serialized.len().try_into().map_err(|_| ValueTooBig)?;

        buf.put_i32(serialized_len);
        buf.extend_from_slice(&serialized);

        Ok(())
    }
}

#[cfg(feature = "num-bigint-04")]
impl Value for num_bigint_04::BigInt {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let serialized = self.to_signed_bytes_be();
        let serialized_len: i32 = serialized.len().try_into().map_err(|_| ValueTooBig)?;

        buf.put_i32(serialized_len);
        buf.extend_from_slice(&serialized);

        Ok(())
    }
}

impl Value for &str {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let str_bytes: &[u8] = self.as_bytes();
        let val_len: i32 = str_bytes.len().try_into().map_err(|_| ValueTooBig)?;

        buf.put_i32(val_len);
        buf.put(str_bytes);

        Ok(())
    }
}

impl Value for Vec<u8> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        <&[u8] as Value>::serialize(&self.as_slice(), buf)
    }
}

impl Value for &[u8] {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let val_len: i32 = self.len().try_into().map_err(|_| ValueTooBig)?;
        buf.put_i32(val_len);

        buf.extend_from_slice(self);

        Ok(())
    }
}

impl<const N: usize> Value for [u8; N] {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let val_len: i32 = self.len().try_into().map_err(|_| ValueTooBig)?;
        buf.put_i32(val_len);

        buf.extend_from_slice(self);

        Ok(())
    }
}

impl Value for IpAddr {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        match self {
            IpAddr::V4(addr) => {
                buf.put_i32(4);
                buf.extend_from_slice(&addr.octets());
            }
            IpAddr::V6(addr) => {
                buf.put_i32(16);
                buf.extend_from_slice(&addr.octets());
            }
        }

        Ok(())
    }
}

impl Value for String {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        <&str as Value>::serialize(&self.as_str(), buf)
    }
}

/// Every `Option<T>` can be serialized as None -> NULL, Some(val) -> val.serialize()
impl<T: Value> Value for Option<T> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        match self {
            Some(val) => <T as Value>::serialize(val, buf),
            None => {
                buf.put_i32(-1);
                Ok(())
            }
        }
    }
}

impl Value for Unset {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        // Unset serializes itself to empty value with length = -2
        buf.put_i32(-2);
        Ok(())
    }
}

impl Value for Counter {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        self.0.serialize(buf)
    }
}

impl Value for CqlDuration {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        let bytes_num_pos: usize = buf.len();
        buf.put_i32(0);

        vint_encode(self.months as i64, buf);
        vint_encode(self.days as i64, buf);
        vint_encode(self.nanoseconds, buf);

        let written_bytes: usize = buf.len() - bytes_num_pos - 4;
        let written_bytes_i32: i32 = written_bytes.try_into().map_err(|_| ValueTooBig)?;
        buf[bytes_num_pos..(bytes_num_pos + 4)].copy_from_slice(&written_bytes_i32.to_be_bytes());

        Ok(())
    }
}

impl<V: Value> Value for MaybeUnset<V> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        match self {
            MaybeUnset::Set(v) => v.serialize(buf),
            MaybeUnset::Unset => Unset.serialize(buf),
        }
    }
}

// Every &impl Value and &dyn Value should also implement Value
impl<T: Value + ?Sized> Value for &T {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        <T as Value>::serialize(*self, buf)
    }
}

// Every Boxed Value should also implement Value
impl<T: Value + ?Sized> Value for Box<T> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        <T as Value>::serialize(self.as_ref(), buf)
    }
}

fn serialize_map<K: Value, V: Value>(
    kv_iter: impl Iterator<Item = (K, V)>,
    kv_count: usize,
    buf: &mut Vec<u8>,
) -> Result<(), ValueTooBig> {
    let bytes_num_pos: usize = buf.len();
    buf.put_i32(0);

    buf.put_i32(kv_count.try_into().map_err(|_| ValueTooBig)?);
    for (key, value) in kv_iter {
        <K as Value>::serialize(&key, buf)?;
        <V as Value>::serialize(&value, buf)?;
    }

    let written_bytes: usize = buf.len() - bytes_num_pos - 4;
    let written_bytes_i32: i32 = written_bytes.try_into().map_err(|_| ValueTooBig)?;
    buf[bytes_num_pos..(bytes_num_pos + 4)].copy_from_slice(&written_bytes_i32.to_be_bytes());

    Ok(())
}

fn serialize_list_or_set<'a, V: 'a + Value>(
    elements_iter: impl Iterator<Item = &'a V>,
    element_count: usize,
    buf: &mut Vec<u8>,
) -> Result<(), ValueTooBig> {
    let bytes_num_pos: usize = buf.len();
    buf.put_i32(0);

    buf.put_i32(element_count.try_into().map_err(|_| ValueTooBig)?);
    for value in elements_iter {
        <V as Value>::serialize(value, buf)?;
    }

    let written_bytes: usize = buf.len() - bytes_num_pos - 4;
    let written_bytes_i32: i32 = written_bytes.try_into().map_err(|_| ValueTooBig)?;
    buf[bytes_num_pos..(bytes_num_pos + 4)].copy_from_slice(&written_bytes_i32.to_be_bytes());

    Ok(())
}

impl<V: Value, S: BuildHasher + Default> Value for HashSet<V, S> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        serialize_list_or_set(self.iter(), self.len(), buf)
    }
}

impl<K: Value, V: Value, S: BuildHasher> Value for HashMap<K, V, S> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        serialize_map(self.iter(), self.len(), buf)
    }
}

impl<V: Value> Value for BTreeSet<V> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        serialize_list_or_set(self.iter(), self.len(), buf)
    }
}

impl<K: Value, V: Value> Value for BTreeMap<K, V> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        serialize_map(self.iter(), self.len(), buf)
    }
}

impl<T: Value> Value for Vec<T> {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        serialize_list_or_set(self.iter(), self.len(), buf)
    }
}

impl<T: Value> Value for &[T] {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        serialize_list_or_set(self.iter(), self.len(), buf)
    }
}

fn serialize_tuple<V: Value>(
    elem_iter: impl Iterator<Item = V>,
    buf: &mut Vec<u8>,
) -> Result<(), ValueTooBig> {
    let bytes_num_pos: usize = buf.len();
    buf.put_i32(0);

    for elem in elem_iter {
        elem.serialize(buf)?;
    }

    let written_bytes: usize = buf.len() - bytes_num_pos - 4;
    let written_bytes_i32: i32 = written_bytes.try_into().map_err(|_| ValueTooBig)?;
    buf[bytes_num_pos..(bytes_num_pos + 4)].copy_from_slice(&written_bytes_i32.to_be_bytes());

    Ok(())
}

fn serialize_empty(buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
    buf.put_i32(0);
    Ok(())
}

impl Value for CqlValue {
    fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
        match self {
            CqlValue::Map(m) => serialize_map(m.iter().map(|p| (&p.0, &p.1)), m.len(), buf),
            CqlValue::Tuple(t) => serialize_tuple(t.iter(), buf),

            // A UDT value is composed of successive [bytes] values, one for each field of the UDT
            // value (in the order defined by the type), so they serialize in a same way tuples do.
            CqlValue::UserDefinedType { fields, .. } => {
                serialize_tuple(fields.iter().map(|(_, value)| value), buf)
            }

            CqlValue::Date(d) => d.serialize(buf),
            CqlValue::Duration(d) => d.serialize(buf),
            CqlValue::Timestamp(t) => t.serialize(buf),
            CqlValue::Time(t) => t.serialize(buf),

            CqlValue::Ascii(s) | CqlValue::Text(s) => s.serialize(buf),
            CqlValue::List(v) | CqlValue::Set(v) => v.serialize(buf),

            CqlValue::Blob(b) => b.serialize(buf),
            CqlValue::Boolean(b) => b.serialize(buf),
            CqlValue::Counter(c) => c.serialize(buf),
            CqlValue::Decimal(d) => d.serialize(buf),
            CqlValue::Double(d) => d.serialize(buf),
            CqlValue::Float(f) => f.serialize(buf),
            CqlValue::Int(i) => i.serialize(buf),
            CqlValue::BigInt(i) => i.serialize(buf),
            CqlValue::Inet(i) => i.serialize(buf),
            CqlValue::SmallInt(s) => s.serialize(buf),
            CqlValue::TinyInt(t) => t.serialize(buf),
            CqlValue::Timeuuid(t) => t.serialize(buf),
            CqlValue::Uuid(u) => u.serialize(buf),
            CqlValue::Varint(v) => v.serialize(buf),

            CqlValue::Empty => serialize_empty(buf),
            CqlValue::Vector(v) => todo!(),
        }
    }
}

macro_rules! impl_value_for_tuple {
    ( $($Ti:ident),* ; $($FieldI:tt),* ) => {
    impl<$($Ti),+> Value for ($($Ti,)+)
        where
            $($Ti: Value),+
        {
            fn serialize(&self, buf: &mut Vec<u8>) -> Result<(), ValueTooBig> {
                let bytes_num_pos: usize = buf.len();
                buf.put_i32(0);
                $(
                    <$Ti as Value>::serialize(&self.$FieldI, buf)?;
                )*

                let written_bytes: usize = buf.len() - bytes_num_pos - 4;
                let written_bytes_i32: i32 = written_bytes.try_into().map_err(|_| ValueTooBig) ?;
                buf[bytes_num_pos..(bytes_num_pos+4)].copy_from_slice(&written_bytes_i32.to_be_bytes());

                Ok(())
            }
        }
    }
}

impl_value_for_tuple!(T0; 0);
impl_value_for_tuple!(T0, T1; 0, 1);
impl_value_for_tuple!(T0, T1, T2; 0, 1, 2);
impl_value_for_tuple!(T0, T1, T2, T3; 0, 1, 2, 3);
impl_value_for_tuple!(T0, T1, T2, T3, T4; 0, 1, 2, 3, 4);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5; 0, 1, 2, 3, 4, 5);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6; 0, 1, 2, 3, 4, 5, 6);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7; 0, 1, 2, 3, 4, 5, 6, 7);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8; 0, 1, 2, 3, 4, 5, 6, 7, 8);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14);
impl_value_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);

//
//  ValueList impls
//

// Implement ValueList for the unit type
impl ValueList for () {
    fn serialized(&self) -> SerializedResult<'_> {
        Ok(Cow::Owned(LegacySerializedValues::new()))
    }
}

// Implement ValueList for &[] - u8 because otherwise rust can't infer type
impl ValueList for [u8; 0] {
    fn serialized(&self) -> SerializedResult<'_> {
        Ok(Cow::Owned(LegacySerializedValues::new()))
    }
}

// Implement ValueList for slices of Value types
impl<T: Value> ValueList for &[T] {
    fn serialized(&self) -> SerializedResult<'_> {
        let size = std::mem::size_of_val(*self);
        let mut result = LegacySerializedValues::with_capacity(size);
        for val in *self {
            result.add_value(val)?;
        }

        Ok(Cow::Owned(result))
    }
}

// Implement ValueList for Vec<Value>
impl<T: Value> ValueList for Vec<T> {
    fn serialized(&self) -> SerializedResult<'_> {
        let slice = self.as_slice();
        let size = std::mem::size_of_val(slice);
        let mut result = LegacySerializedValues::with_capacity(size);
        for val in self {
            result.add_value(val)?;
        }

        Ok(Cow::Owned(result))
    }
}

// Implement ValueList for maps, which serializes named values
macro_rules! impl_value_list_for_btree_map {
    ($key_type:ty) => {
        impl<T: Value> ValueList for BTreeMap<$key_type, T> {
            fn serialized(&self) -> SerializedResult<'_> {
                let mut result = LegacySerializedValues::with_capacity(self.len());
                for (key, val) in self {
                    result.add_named_value(key, val)?;
                }

                Ok(Cow::Owned(result))
            }
        }
    };
}

// Implement ValueList for maps, which serializes named values
macro_rules! impl_value_list_for_hash_map {
    ($key_type:ty) => {
        impl<T: Value, S: BuildHasher> ValueList for HashMap<$key_type, T, S> {
            fn serialized(&self) -> SerializedResult<'_> {
                let mut result = LegacySerializedValues::with_capacity(self.len());
                for (key, val) in self {
                    result.add_named_value(key, val)?;
                }

                Ok(Cow::Owned(result))
            }
        }
    };
}

impl_value_list_for_hash_map!(String);
impl_value_list_for_hash_map!(&str);
impl_value_list_for_btree_map!(String);
impl_value_list_for_btree_map!(&str);

// Implement ValueList for tuples of Values of size up to 16

// Here is an example implementation for (T0, )
// Further variants are done using a macro
impl<T0: Value> ValueList for (T0,) {
    fn serialized(&self) -> SerializedResult<'_> {
        let size = std::mem::size_of_val(self);
        let mut result = LegacySerializedValues::with_capacity(size);
        result.add_value(&self.0)?;
        Ok(Cow::Owned(result))
    }
}

macro_rules! impl_value_list_for_tuple {
    ( $($Ti:ident),* ; $($FieldI:tt),*) => {
        impl<$($Ti),+> ValueList for ($($Ti,)+)
        where
            $($Ti: Value),+
        {
            fn serialized(&self) -> SerializedResult<'_> {
                let size = std::mem::size_of_val(self);
                let mut result = LegacySerializedValues::with_capacity(size);
                $(
                    result.add_value(&self.$FieldI) ?;
                )*
                Ok(Cow::Owned(result))
            }
        }
    }
}

impl_value_list_for_tuple!(T0, T1; 0, 1);
impl_value_list_for_tuple!(T0, T1, T2; 0, 1, 2);
impl_value_list_for_tuple!(T0, T1, T2, T3; 0, 1, 2, 3);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4; 0, 1, 2, 3, 4);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5; 0, 1, 2, 3, 4, 5);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6; 0, 1, 2, 3, 4, 5, 6);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7; 0, 1, 2, 3, 4, 5, 6, 7);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8; 0, 1, 2, 3, 4, 5, 6, 7, 8);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9; 0, 1, 2, 3, 4, 5, 6, 7, 8, 9);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14);
impl_value_list_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15;
                           0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);

// Every &impl ValueList should also implement ValueList
impl<T: ValueList> ValueList for &T {
    fn serialized(&self) -> SerializedResult<'_> {
        <T as ValueList>::serialized(*self)
    }
}

impl ValueList for LegacySerializedValues {
    fn serialized(&self) -> SerializedResult<'_> {
        Ok(Cow::Borrowed(self))
    }
}

impl<'b> ValueList for Cow<'b, LegacySerializedValues> {
    fn serialized(&self) -> SerializedResult<'_> {
        Ok(Cow::Borrowed(self.as_ref()))
    }
}

//
// BatchValues impls
//

/// Implements `BatchValues` from an `Iterator` over references to things that implement `ValueList`
///
/// This is to avoid requiring allocating a new `Vec` containing all the `ValueList`s directly:
/// with this, one can write:
/// `session.batch(&batch, BatchValuesFromIterator::from(lines_to_insert.iter().map(|l| &l.value_list)))`
/// where `lines_to_insert` may also contain e.g. data to pick the statement...
///
/// The underlying iterator will always be cloned at least once, once to compute the length if it can't be known
/// in advance, and be re-cloned at every retry.
/// It is consequently expected that the provided iterator is cheap to clone (e.g. `slice.iter().map(...)`).
pub struct LegacyBatchValuesFromIter<'a, IT> {
    it: IT,
    _spooky: std::marker::PhantomData<&'a ()>,
}

impl<'a, IT, VL> LegacyBatchValuesFromIter<'a, IT>
where
    IT: Iterator<Item = &'a VL> + Clone,
    VL: ValueList + 'a,
{
    pub fn new(into_iter: impl IntoIterator<IntoIter = IT>) -> Self {
        Self {
            it: into_iter.into_iter(),
            _spooky: std::marker::PhantomData,
        }
    }
}

impl<'a, IT, VL> From<IT> for LegacyBatchValuesFromIter<'a, IT>
where
    IT: Iterator<Item = &'a VL> + Clone,
    VL: ValueList + 'a,
{
    fn from(it: IT) -> Self {
        Self::new(it)
    }
}

impl<'a, IT, VL> LegacyBatchValues for LegacyBatchValuesFromIter<'a, IT>
where
    IT: Iterator<Item = &'a VL> + Clone,
    VL: ValueList + 'a,
{
    type LegacyBatchValuesIter<'r> = LegacyBatchValuesIteratorFromIterator<IT> where Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
        self.it.clone().into()
    }
}

// Implement BatchValues for slices of ValueList types
impl<T: ValueList> LegacyBatchValues for [T] {
    type LegacyBatchValuesIter<'r> = LegacyBatchValuesIteratorFromIterator<std::slice::Iter<'r, T>> where Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
        self.iter().into()
    }
}

// Implement BatchValues for Vec<ValueList>
impl<T: ValueList> LegacyBatchValues for Vec<T> {
    type LegacyBatchValuesIter<'r> = LegacyBatchValuesIteratorFromIterator<std::slice::Iter<'r, T>> where Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
        LegacyBatchValues::batch_values_iter(self.as_slice())
    }
}

// Here is an example implementation for (T0, )
// Further variants are done using a macro
impl<T0: ValueList> LegacyBatchValues for (T0,) {
    type LegacyBatchValuesIter<'r> = LegacyBatchValuesIteratorFromIterator<std::iter::Once<&'r T0>> where Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
        std::iter::once(&self.0).into()
    }
}

pub struct TupleValuesIter<'a, T> {
    tuple: &'a T,
    idx: usize,
}

macro_rules! impl_batch_values_for_tuple {
    ( $($Ti:ident),* ; $($FieldI:tt),* ; $TupleSize:tt) => {
        impl<$($Ti),+> LegacyBatchValues for ($($Ti,)+)
        where
            $($Ti: ValueList),+
        {
            type LegacyBatchValuesIter<'r> = TupleValuesIter<'r, ($($Ti,)+)> where Self: 'r;
            fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
                TupleValuesIter {
                    tuple: self,
                    idx: 0,
                }
            }
        }
        impl<'r, $($Ti),+> LegacyBatchValuesIterator<'r> for TupleValuesIter<'r, ($($Ti,)+)>
        where
            $($Ti: ValueList),+
        {
            fn next_serialized(&mut self) -> Option<SerializedResult<'r>> {
                let serialized_value_res = match self.idx {
                    $(
                        $FieldI => self.tuple.$FieldI.serialized(),
                    )*
                    _ => return None,
                };
                self.idx += 1;
                Some(serialized_value_res)
            }
            fn write_next_to_request(
                &mut self,
                buf: &mut impl BufMut,
            ) -> Option<Result<(), SerializeValuesError>> {
                let ret = match self.idx {
                    $(
                        $FieldI => self.tuple.$FieldI.write_to_request(buf),
                    )*
                    _ => return None,
                };
                self.idx += 1;
                Some(ret)
            }
            fn skip_next(&mut self) -> Option<()> {
                if self.idx < $TupleSize {
                    self.idx += 1;
                    Some(())
                } else {
                    None
                }
            }
        }
    }
}

impl_batch_values_for_tuple!(T0, T1; 0, 1; 2);
impl_batch_values_for_tuple!(T0, T1, T2; 0, 1, 2; 3);
impl_batch_values_for_tuple!(T0, T1, T2, T3; 0, 1, 2, 3; 4);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4; 0, 1, 2, 3, 4; 5);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5; 0, 1, 2, 3, 4, 5; 6);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6; 0, 1, 2, 3, 4, 5, 6; 7);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7; 0, 1, 2, 3, 4, 5, 6, 7; 8);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8; 0, 1, 2, 3, 4, 5, 6, 7, 8; 9);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9; 10);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10; 11);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11; 12);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12; 13);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13; 14);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14; 15);
impl_batch_values_for_tuple!(T0, T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15;
                             0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15; 16);

// Every &impl BatchValues should also implement BatchValues
impl<'a, T: LegacyBatchValues + ?Sized> LegacyBatchValues for &'a T {
    type LegacyBatchValuesIter<'r> = <T as LegacyBatchValues>::LegacyBatchValuesIter<'r> where Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
        <T as LegacyBatchValues>::batch_values_iter(*self)
    }
}

/// Allows reusing already-serialized first value
///
/// We'll need to build a `LegacySerializedValues` for the first ~`ValueList` of a batch to figure out the shard (#448).
/// Once that is done, we can use that instead of re-serializing.
///
/// This struct implements both `BatchValues` and `BatchValuesIterator` for that purpose
pub struct LegacyBatchValuesFirstSerialized<'f, T> {
    first: Option<&'f LegacySerializedValues>,
    rest: T,
}

impl<'f, T: LegacyBatchValues> LegacyBatchValuesFirstSerialized<'f, T> {
    pub fn new(
        batch_values: T,
        already_serialized_first: Option<&'f LegacySerializedValues>,
    ) -> Self {
        Self {
            first: already_serialized_first,
            rest: batch_values,
        }
    }
}

impl<'f, BV: LegacyBatchValues> LegacyBatchValues for LegacyBatchValuesFirstSerialized<'f, BV> {
    type LegacyBatchValuesIter<'r> =
        LegacyBatchValuesFirstSerialized<'f, <BV as LegacyBatchValues>::LegacyBatchValuesIter<'r>> where Self: 'r;
    fn batch_values_iter(&self) -> Self::LegacyBatchValuesIter<'_> {
        LegacyBatchValuesFirstSerialized {
            first: self.first,
            rest: self.rest.batch_values_iter(),
        }
    }
}

impl<'a, 'f: 'a, IT: LegacyBatchValuesIterator<'a>> LegacyBatchValuesIterator<'a>
    for LegacyBatchValuesFirstSerialized<'f, IT>
{
    fn next_serialized(&mut self) -> Option<SerializedResult<'a>> {
        match self.first.take() {
            Some(first) => {
                self.rest.skip_next();
                Some(Ok(Cow::Borrowed(first)))
            }
            None => self.rest.next_serialized(),
        }
    }
    fn write_next_to_request(
        &mut self,
        buf: &mut impl BufMut,
    ) -> Option<Result<(), SerializeValuesError>> {
        match self.first.take() {
            Some(first) => {
                self.rest.skip_next();
                first.write_to_request(buf);
                Some(Ok(()))
            }
            None => self.rest.write_next_to_request(buf),
        }
    }
    fn skip_next(&mut self) -> Option<()> {
        self.rest.skip_next();
        self.first.take().map(|_| ())
    }
}
