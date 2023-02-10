use super::construct_event_closure;
use crate::indexed_db::db_driver::{InternalItem, ItemId};
use crate::indexed_db::BeBigUint;
use async_trait::async_trait;
use common::wasm::{deserialize_from_js, serialize_to_js, stringify_js_error};
use derive_more::Display;
use futures::channel::mpsc;
use futures::StreamExt;
use js_sys::Array;
use mm2_err_handle::prelude::*;
use serde_json::{self as json, Value as Json};
use std::convert::TryInto;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{IdbCursorWithValue, IdbIndex, IdbKeyRange, IdbRequest};

mod multi_key_bound_cursor;
mod multi_key_cursor;
mod single_key_bound_cursor;
mod single_key_cursor;

pub use multi_key_bound_cursor::IdbMultiKeyBoundCursor;
pub use multi_key_cursor::IdbMultiKeyCursor;
pub use single_key_bound_cursor::IdbSingleKeyBoundCursor;
pub use single_key_cursor::IdbSingleKeyCursor;

pub type CursorResult<T> = Result<T, MmError<CursorError>>;
pub type DbFilter = Box<dyn FnMut(&Json) -> (CollectItemAction, CollectCursorAction) + Send>;

#[derive(Debug, Display, PartialEq)]
pub enum CursorError {
    #[display(
        fmt = "Error serializing the '{}' value of the index field '{}' : {:?}",
        value,
        field,
        description
    )]
    ErrorSerializingIndexFieldValue {
        field: String,
        value: String,
        description: String,
    },
    #[display(fmt = "Error deserializing the an index key: {:?}", description)]
    ErrorDeserializingIndexValue { description: String },
    #[display(fmt = "Error deserializing an item: {:?}", _0)]
    ErrorDeserializingItem(String),
    #[display(fmt = "Error opening cursor: {:?}", description)]
    ErrorOpeningCursor { description: String },
    #[display(fmt = "Cursor advance error: {:?}", description)]
    AdvanceError { description: String },
    #[display(fmt = "Invalid key range: {:?}", description)]
    InvalidKeyRange { description: String },
    #[display(fmt = "Type mismatch: expected '{}', found '{}'", expected, found)]
    TypeMismatch { expected: String, found: String },
    #[display(
        fmt = "Incorrect number of keys per a DB index: expected '{}', found '{}'",
        expected,
        found
    )]
    IncorrectNumberOfKeysPerIndex { expected: usize, found: usize },
    #[display(fmt = "Error occurred due to an unexpected state: {:?}", _0)]
    UnexpectedState(String),
    #[display(fmt = "Incorrect usage of the cursor: {:?}", description)]
    IncorrectUsage { description: String },
}

impl CursorError {
    fn type_mismatch(expected: &str, found: &Json) -> CursorError {
        CursorError::TypeMismatch {
            expected: expected.to_owned(),
            found: format!("{:?}", found),
        }
    }
}

/// The value types that are guaranteed ordered as we expect.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CursorBoundValue {
    Uint(u32),
    Int(i32),
    BigUint(BeBigUint),
}

#[derive(Default)]
pub struct CursorFilters {
    pub(crate) only_keys: Vec<(String, Json)>,
    pub(crate) bound_keys: Vec<(String, CursorBoundValue, CursorBoundValue)>,
}

impl From<u32> for CursorBoundValue {
    fn from(uint: u32) -> Self { CursorBoundValue::Uint(uint) }
}

impl From<i32> for CursorBoundValue {
    fn from(int: i32) -> Self { CursorBoundValue::Int(int) }
}

impl From<u64> for CursorBoundValue {
    fn from(uint: u64) -> Self { CursorBoundValue::BigUint(BeBigUint::from(uint)) }
}

impl From<usize> for CursorBoundValue {
    fn from(uint: usize) -> Self { CursorBoundValue::BigUint(BeBigUint::from(uint)) }
}

impl From<u128> for CursorBoundValue {
    fn from(uint: u128) -> Self { CursorBoundValue::BigUint(BeBigUint::from(uint)) }
}

impl From<BeBigUint> for CursorBoundValue {
    fn from(uint: BeBigUint) -> Self { CursorBoundValue::BigUint(uint) }
}

impl CursorBoundValue {
    fn next(&self) -> CursorBoundValue {
        match self {
            CursorBoundValue::Uint(uint) => CursorBoundValue::Uint(*uint + 1),
            CursorBoundValue::Int(int) => CursorBoundValue::Int(*int + 1),
            CursorBoundValue::BigUint(int) => CursorBoundValue::BigUint(int.clone() + 1u64),
        }
    }

    pub fn to_js_value(&self) -> CursorResult<JsValue> {
        match self {
            CursorBoundValue::Uint(uint) => Ok(JsValue::from(*uint as u32)),
            CursorBoundValue::Int(int) => Ok(JsValue::from(*int as i32)),
            CursorBoundValue::BigUint(int) => serialize_to_js(int).map_to_mm(|e| CursorError::InvalidKeyRange {
                description: e.to_string(),
            }),
        }
    }

    fn same_inner_type(&self, other: &Self) -> bool {
        // `matches` macro leads to the following error:
        // (CursorBoundValue::Uint(_), CursorBoundValue::Uint(_))
        // ^ no rules expected this token in macro call
        match (self, other) {
            (CursorBoundValue::Int(_), CursorBoundValue::Int(_))
            | (CursorBoundValue::Uint(_), CursorBoundValue::Uint(_))
            | (CursorBoundValue::BigUint(_), CursorBoundValue::BigUint(_)) => true,
            _ => false,
        }
    }

    fn deserialize_with_expected_type(value: &Json, expected: &Self) -> CursorResult<CursorBoundValue> {
        match expected {
            CursorBoundValue::Uint(_) => {
                let uint64 = value.as_u64().or_mm_err(|| CursorError::type_mismatch("u32", value))?;
                let uint = uint64
                    .try_into()
                    .map_to_mm(|_| CursorError::type_mismatch("u32", value))?;
                Ok(CursorBoundValue::Uint(uint))
            },
            CursorBoundValue::Int(_) => {
                let int64 = value.as_i64().or_mm_err(|| CursorError::type_mismatch("i32", value))?;
                let int = int64
                    .try_into()
                    .map_to_mm(|_| CursorError::type_mismatch("i32", value))?;
                Ok(CursorBoundValue::Int(int))
            },
            CursorBoundValue::BigUint(_) => json::from_value::<BeBigUint>(value.clone())
                .map(CursorBoundValue::BigUint)
                .map_to_mm(|_| CursorError::type_mismatch("BeBigUint", value)),
        }
    }
}

/// TODO rename to `CursorAction`.
#[derive(Debug, PartialEq)]
pub enum CollectCursorAction {
    Continue,
    ContinueWithValue(JsValue),
    Stop,
}

/// TODO rename to `IterItemAction`.
#[derive(Debug, PartialEq)]
pub enum CollectItemAction {
    Include,
    Skip,
}

pub trait CursorDriverImpl: Sized {
    fn key_range(&self) -> CursorResult<Option<IdbKeyRange>>;

    fn on_iteration(&mut self, key: JsValue) -> CursorResult<(CollectItemAction, CollectCursorAction)>;
}

pub(crate) struct CursorDriver {
    /// An actual cursor implementation.
    inner: IdbCursorEnum,
    db_index: IdbIndex,
    cursor_request: IdbRequest,
    cursor_item_rx: mpsc::Receiver<Result<JsValue, JsValue>>,
    finished: bool,
    /// We need to hold the closures in memory till `cursor` exists.
    _onsuccess_closure: Closure<dyn FnMut()>,
    _onerror_closure: Closure<dyn FnMut()>,
}

impl CursorDriver {
    pub async fn init_cursor(db_index: IdbIndex, filters: CursorFilters) -> CursorResult<CursorDriver> {
        let inner = IdbCursorEnum::new(filters);

        let cursor_request_result = match inner.key_range()? {
            Some(key_range) => db_index.open_cursor_with_range(&key_range),
            None => db_index.open_cursor(),
        };
        let cursor_request = cursor_request_result.map_err(|e| CursorError::ErrorOpeningCursor {
            description: stringify_js_error(&e),
        })?;

        let (cursor_item_tx, cursor_item_rx) = mpsc::channel(1);

        let onsuccess_closure = construct_event_closure(Ok, cursor_item_tx.clone());
        let onerror_closure = construct_event_closure(Err, cursor_item_tx);

        cursor_request.set_onsuccess(Some(onsuccess_closure.as_ref().unchecked_ref()));
        cursor_request.set_onerror(Some(onerror_closure.as_ref().unchecked_ref()));

        Ok(CursorDriver {
            inner: IdbCursorEnum::new(filters),
            db_index,
            cursor_request,
            cursor_item_rx,
            _onsuccess_closure: onsuccess_closure,
            _onerror_closure: onerror_closure,
        })
    }

    pub(crate) async fn next(&mut self) -> CursorResult<Option<(ItemId, Json)>> {
        while Some(event) = self.cursor_item_rx.next().await {
            let _cursor_event = event.map_to_mm(|e| CursorError::ErrorOpeningCursor {
                description: stringify_js_error(&e),
            })?;

            let cursor = match cursor_from_request(&self.cursor_request)? {
                Some(cursor) => cursor,
                // No more items.
                None => {
                    self.finished = true;
                    return Ok(None);
                },
            };

            let (key, js_value) = match (cursor.key(), cursor.value()) {
                (Ok(key), Ok(js_value)) => (key, js_value),
                // No more items.
                _ => {
                    self.finished = true;
                    return Ok(None);
                },
            };

            let item: InternalItem =
                deserialize_from_js(js_value).map_to_mm(|e| CursorError::ErrorDeserializingItem(e.to_string()))?;

            let (item_action, cursor_action) = self.on_collect_iter(key, &item.item)?;

            let to_return = match item_action {
                CollectItemAction::Include => Some(item.into_pair()),
                CollectItemAction::Skip => None,
            };

            match cursor_action {
                CollectCursorAction::Continue => cursor.continue_().map_to_mm(|e| CursorError::AdvanceError {
                    description: stringify_js_error(&e),
                })?,
                CollectCursorAction::ContinueWithValue(next_value) => {
                    cursor
                        .continue_with_key(&next_value)
                        .map_to_mm(|e| CursorError::AdvanceError {
                            description: stringify_js_error(&e),
                        })?
                },
                // don't advance the cursor, just stop the loop
                CollectCursorAction::Stop => None,
            }
        }

        // No more items.
        Ok(None)
    }
}

#[async_trait(?Send)]
pub trait CursorOps: Sized {
    fn db_index(&self) -> &IdbIndex;

    fn key_range(&self) -> CursorResult<Option<IdbKeyRange>>;

    fn on_collect_iter(&mut self, key: JsValue, value: &Json)
        -> CursorResult<(CollectItemAction, CollectCursorAction)>;

    /// Collect items that match the specified bounds.
    async fn collect(mut self) -> CursorResult<Vec<(ItemId, Json)>> {
        let (tx, mut rx) = mpsc::channel(1);

        let db_index = self.db_index();
        let cursor_request_result = match self.key_range()? {
            Some(key_range) => db_index.open_cursor_with_range(&key_range),
            None => db_index.open_cursor(),
        };
        let cursor_request = cursor_request_result.map_err(|e| CursorError::ErrorOpeningCursor {
            description: stringify_js_error(&e),
        })?;

        let onsuccess_closure = construct_event_closure(Ok, tx.clone());
        let onerror_closure = construct_event_closure(Err, tx);

        cursor_request.set_onsuccess(Some(onsuccess_closure.as_ref().unchecked_ref()));
        cursor_request.set_onerror(Some(onerror_closure.as_ref().unchecked_ref()));

        let mut collected_items = Vec::new();

        while let Some(event) = rx.next().await {
            let _cursor_event = event.map_to_mm(|e| CursorError::ErrorOpeningCursor {
                description: stringify_js_error(&e),
            })?;

            let cursor = match cursor_from_request(&cursor_request)? {
                Some(cursor) => cursor,
                // no more items, stop the loop
                None => break,
            };

            let (key, js_value) = match (cursor.key(), cursor.value()) {
                (Ok(key), Ok(js_value)) => (key, js_value),
                // no more items, stop the loop
                _ => break,
            };

            let item: InternalItem =
                deserialize_from_js(js_value).map_to_mm(|e| CursorError::ErrorDeserializingItem(e.to_string()))?;

            let (item_action, cursor_action) = self.on_collect_iter(key, &item.item)?;
            match item_action {
                CollectItemAction::Include => collected_items.push(item.into_pair()),
                CollectItemAction::Skip => (),
            }

            match cursor_action {
                CollectCursorAction::Continue => cursor.continue_().map_to_mm(|e| CursorError::AdvanceError {
                    description: stringify_js_error(&e),
                })?,
                CollectCursorAction::ContinueWithValue(next_value) => {
                    cursor
                        .continue_with_key(&next_value)
                        .map_to_mm(|e| CursorError::AdvanceError {
                            description: stringify_js_error(&e),
                        })?
                },
                // don't advance the cursor, just stop the loop
                CollectCursorAction::Stop => break,
            }
        }

        Ok(collected_items)
    }
}

pub(crate) enum IdbCursorEnum {
    SingleKey(IdbSingleKeyCursor),
    SingleKeyBound(IdbSingleKeyBoundCursor),
    MultiKey(IdbMultiKeyCursor),
    MultiKeyBound(IdbMultiKeyBoundCursor),
}

impl IdbCursorEnum {
    fn new(cursor_filters: CursorFilters) -> IdbCursorEnum {
        if cursor_filters.only_keys.len() > 1 && cursor_filters.bound_keys.is_empty() {
            return IdbCursorEnum::MultiKey(IdbMultiKeyCursor::new(cursor_filters.only_keys));
        }
        if cursor_filters.only_keys.len() + cursor_filters.bound_keys.len() > 1 {
            return IdbCursorEnum::MultiKeyBound(IdbMultiKeyBoundCursor::new(
                cursor_filters.only_keys,
                cursor_filters.bound_keys,
            ));
        } // otherwise we're sure that there is either one `only`, or one `bound`, or no constraint specified.

        if let Some((field_name, field_value)) = cursor_filters.only_keys.into_iter().next() {
            return IdbCursorEnum::SingleKey(IdbSingleKeyCursor::new(field_name, field_value));
        }

        if let Some((field_name, lower_bound, upper_bound)) = cursor_filters.bound_keys.into_iter().next() {
            IdbCursorEnum::SingleKeyBound(IdbSingleKeyBoundCursor::new(field_name, lower_bound, upper_bound))
        }

        todo!()
    }
}

impl CursorDriverImpl for IdbCursorEnum {
    fn key_range(&self) -> CursorResult<Option<IdbKeyRange>> {
        match self {
            IdbCursorEnum::SingleKey(single) => single.key_range(),
            IdbCursorEnum::SingleKeyBound(single_bound) => single_bound.key_range(),
            IdbCursorEnum::MultiKey(multi) => multi.key_range(),
            IdbCursorEnum::MultiKeyBound(multi_bound) => multi_bound.key_range(),
        }
    }

    fn on_iteration(&mut self, key: JsValue) -> CursorResult<(CollectItemAction, CollectCursorAction)> {
        match self {
            IdbCursorEnum::SingleKey(single) => single.on_iteration(key),
            IdbCursorEnum::SingleKeyBound(single_bound) => single_bound.on_iteration(key),
            IdbCursorEnum::MultiKey(multi) => multi.on_iteration(key),
            IdbCursorEnum::MultiKeyBound(multi_bound) => multi_bound.on_iteration(key),
        }
    }
}

fn index_key_as_array(index_key: JsValue) -> CursorResult<Array> {
    index_key.dyn_into::<Array>().map_err(|index_key| {
        MmError::new(CursorError::TypeMismatch {
            expected: "js_sys::Array".to_owned(),
            found: format!("{:?}", index_key),
        })
    })
}

fn cursor_from_request(request: &IdbRequest) -> CursorResult<Option<IdbCursorWithValue>> {
    let db_result = request
        .result()
        .map_to_mm(|e| CursorError::UnexpectedState(stringify_js_error(&e)))?;
    if db_result.is_null() {
        return Ok(None);
    }
    db_result
        .dyn_into::<IdbCursorWithValue>()
        .map(Some)
        .map_to_mm(|db_result| CursorError::TypeMismatch {
            expected: "IdbCursorWithValue".to_owned(),
            found: format!("{:?}", db_result),
        })
}
