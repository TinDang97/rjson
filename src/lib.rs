use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use pyo3::types::{PyDict, PyList, PyBool, PyFloat, PyLong, PyString, PyTuple, PyAny}; // PyO3 types for Python object interaction
use serde_json;
use serde::de::{self, Visitor, MapAccess, SeqAccess, Deserializer};
use serde::ser::{Serialize, Serializer, SerializeMap, SerializeSeq};
use std::fmt;
use serde::de::DeserializeSeed; // Fix: bring trait into scope

#[allow(dead_code)]
/// Utility: Converts a Rust `serde_json::Value` into its corresponding Python object representation.
///
/// This function is not currently used, but is retained for potential future use or testing.
///
/// # Arguments
/// * `py` - The Python GIL token.
/// * `value` - The `serde_json::Value` to convert.
///
/// # Returns
/// A `PyObject` representing the JSON value, or a `PyValueError` on error.
fn serde_value_to_py_object(py: Python, value: serde_json::Value) -> PyResult<PyObject> {
    match value {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => Ok(b.to_object(py)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.to_object(py))
            } else if let Some(u) = n.as_u64() {
                // Python doesn't distinguish u64/i64 in the same way,
                // but u64 can be larger than i64::MAX.
                // to_object will handle large integers correctly.
                Ok(u.to_object(py))
            } else if let Some(f) = n.as_f64() {
                Ok(f.to_object(py))
            } else {
                Err(PyValueError::new_err(format!("Unsupported number value: {}", n)))
            }
        }
        serde_json::Value::String(s) => Ok(s.to_object(py)),
        serde_json::Value::Array(arr) => {
            let mut py_elements = Vec::with_capacity(arr.len());
            for item in arr {
                py_elements.push(serde_value_to_py_object(py, item)?);
            }
            Ok(PyList::new_bound(py, py_elements).into())
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new_bound(py);
            for (k, v) in map {
                dict.set_item(k, serde_value_to_py_object(py, v)?)?;
            }
            Ok(dict.into())
        }
    }
}

#[allow(dead_code)]
/// Utility: Converts a Python object (PyAny) to a serde_json::Value recursively.
///
/// This function is not currently used, but is retained for potential future use or testing.
///
/// # Arguments
/// * `py` - The Python GIL token.
/// * `obj` - The Python object to convert.
///
/// # Returns
/// A serde_json::Value representing the Python object.
///
/// # Errors
/// Returns a PyValueError if the object cannot be converted.
fn py_object_to_serde_value(py: Python, obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    if obj.is_none() {
        Ok(serde_json::Value::Null)
    } else if let Ok(b_val) = obj.downcast_exact::<PyBool>() {
        Ok(serde_json::Value::Bool(b_val.is_true()))
    } else if let Ok(l_val) = obj.downcast_exact::<PyLong>() {
        // Attempt to extract as i64 or u64 first for efficiency.
        if let Ok(val_i64) = l_val.extract::<i64>() {
            Ok(serde_json::Number::from(val_i64).into())
        } else if let Ok(val_u64) = l_val.extract::<u64>() {
            Ok(serde_json::Number::from(val_u64).into())
        } else {
            // For very large integers that don't fit in i64/u64,
            // attempt to convert to f64 if it's a round number.
            // Otherwise, convert to a string and then parse to serde_json::Number
            // to preserve precision, as Python integers can be arbitrarily large.
            if let Ok(val_f64) = l_val.extract::<f64>() {
                if val_f64.fract() == 0.0 { // Check if it's a whole number
                    if let Some(num) = serde_json::Number::from_f64(val_f64) {
                        return Ok(serde_json::Value::Number(num));
                    }
                }
            }
            let s = l_val.to_string();
            match serde_json::from_str::<serde_json::Number>(&s) {
                Ok(num) => Ok(serde_json::Value::Number(num)),
                Err(_) => Err(PyValueError::new_err(format!(
                    "Could not convert Python integer {} to a JSON number", s
                ))),
            }
        }
    } else if let Ok(f_val) = obj.downcast_exact::<PyFloat>() {
        let val_f64 = f_val.extract::<f64>()?;
        // Convert f64 to serde_json::Number, handling potential non-finite values.
        serde_json::Number::from_f64(val_f64)
            .map(serde_json::Value::Number)
            .ok_or_else(|| PyValueError::new_err(format!("Invalid Python float value for JSON: {} (e.g. NaN or Infinity)", val_f64)))
    } else if let Ok(s_val) = obj.downcast_exact::<PyString>() {
        Ok(serde_json::Value::String(s_val.to_str()?.to_owned()))
    } else if let Ok(list_val) = obj.downcast_exact::<PyList>() {
        let mut vec = Vec::with_capacity(list_val.len());
        for item_bound_res in list_val.iter() {
            // Assuming item_bound_res is always Ok based on PyList::iter behavior.
            // If PyList::iter could yield Err, proper error handling would be needed here.
            let item_bound = item_bound_res; 
            vec.push(py_object_to_serde_value(py, &item_bound)?);
        }
        Ok(serde_json::Value::Array(vec))
    } else if let Ok(tuple_val) = obj.downcast_exact::<PyTuple>() { // Treat Python tuples like JSON arrays.
        let mut vec = Vec::with_capacity(tuple_val.len());
        for item_bound_res in tuple_val.iter() {
            // Assuming item_bound_res is always Ok based on PyTuple::iter behavior.
            let item_bound = item_bound_res; 
            vec.push(py_object_to_serde_value(py, &item_bound)?);
        }
        Ok(serde_json::Value::Array(vec))
    } else if let Ok(dict_val) = obj.downcast_exact::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (key_bound, value_bound_res) in dict_val.iter() {
            // Assuming value_bound_res is always Ok based on PyDict::iter behavior.
            let value_bound = value_bound_res; 
            let key_str = key_bound.extract::<String>()
                .map_err(|e| PyValueError::new_err(format!("Dictionary keys must be strings for JSON serialization: {}", e)))?;
            map.insert(key_str, py_object_to_serde_value(py, &value_bound)?);
        }
        Ok(serde_json::Value::Object(map))
    } else {
        Err(PyValueError::new_err(format!(
            "Unsupported Python type for JSON serialization: {}",
            obj.get_type().name()?
        )))
    }
}

/// Visitor that builds PyO3 objects directly from serde_json events.
///
/// This is a high-performance path that avoids intermediate allocations by constructing
/// Python objects as the JSON is parsed. It is not as fast as orjson due to PyO3 and GIL overhead.
struct PyObjectVisitor<'py> {
    py: Python<'py>,
}

impl<'de, 'py> Visitor<'de> for PyObjectVisitor<'py> {
    type Value = PyObject;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(self.py.None())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(self.py.None())
    }
    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PyObjectVisitor { py: self.py })
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        // Preallocate for performance, batch-create PyList after collecting elements.
        let size = seq.size_hint().unwrap_or(0);
        let mut elements = Vec::with_capacity(size);
        while let Some(elem) = seq.next_element_seed(PyObjectSeed { py: self.py })? {
            elements.push(elem);
        }
        // PyList::new_bound is the correct method for PyO3 0.19+
        let pylist = PyList::new_bound(self.py, &elements).into();
        Ok(pylist)
    }
    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // Batch-collect keys and values, then build PyDict in one go for performance.
        let mut keys = Vec::new();
        let mut values = Vec::new();
        while let Some((key, value)) = map.next_entry_seed(KeySeed, PyObjectSeed { py: self.py })? {
            keys.push(key);
            values.push(value);
        }
        let dict = {
            let dict = PyDict::new_bound(self.py);
            for (k, v) in keys.iter().zip(values.iter()) {
                dict.set_item(k, v).unwrap();
            }
            dict.into()
        };
        Ok(dict)
    }
}

struct PyObjectSeed<'py> {
    py: Python<'py>,
}

impl<'de, 'py> de::DeserializeSeed<'de> for PyObjectSeed<'py> {
    type Value = PyObject;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PyObjectVisitor { py: self.py })
    }
}

struct KeySeed;
impl<'de> de::DeserializeSeed<'de> for KeySeed {
    type Value = String;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        de::Deserialize::deserialize(deserializer)
    }
}

/// Parses a JSON string into a Python object.
///
/// Supported Python types: dict, list, str, int, float, bool, and None.
///
/// # Arguments
/// * `json_str` - The JSON string to parse.
///
/// # Returns
/// A PyObject representing the parsed JSON, or a PyValueError on error.
#[pyfunction]
fn loads(json_str: &str) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let mut de = serde_json::Deserializer::from_str(json_str);
        // Fix: use trait method from DeserializeSeed
        DeserializeSeed::deserialize(PyObjectSeed { py }, &mut de)
            .map_err(|e| PyValueError::new_err(format!("JSON parsing error: {e}")))
    })
}

/// Wrapper to implement serde::Serialize for PyAny (Python objects).
///
/// This enables direct serialization of Python objects to JSON using serde_json,
/// bypassing intermediate conversions for performance.
struct PyAnySerialize<'py> {
    obj: &'py Bound<'py, PyAny>,
}

impl<'py> Serialize for PyAnySerialize<'py> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let obj = self.obj;
        if obj.is_none() {
            serializer.serialize_unit()
        } else if let Ok(b_val) = obj.downcast_exact::<PyBool>() {
            serializer.serialize_bool(b_val.is_true())
        } else if let Ok(l_val) = obj.downcast_exact::<PyLong>() {
            if let Ok(val_i64) = l_val.extract::<i64>() {
                serializer.serialize_i64(val_i64)
            } else if let Ok(val_u64) = l_val.extract::<u64>() {
                serializer.serialize_u64(val_u64)
            } else if let Ok(val_f64) = l_val.extract::<f64>() {
                if val_f64.fract() == 0.0 {
                    serializer.serialize_f64(val_f64)
                } else {
                    Err(serde::ser::Error::custom("Cannot serialize non-integer PyLong as JSON number"))
                }
            } else {
                let s = l_val.to_string();
                serializer.serialize_str(&s)
            }
        } else if let Ok(f_val) = obj.downcast_exact::<PyFloat>() {
            let val_f64 = f_val.extract::<f64>().map_err(serde::ser::Error::custom)?;
            serializer.serialize_f64(val_f64)
        } else if let Ok(s_val) = obj.downcast_exact::<PyString>() {
            serializer.serialize_str(s_val.to_str().map_err(serde::ser::Error::custom)?)
        } else if let Ok(list_val) = obj.downcast_exact::<PyList>() {
            let mut seq = serializer.serialize_seq(Some(list_val.len()))?;
            for item in list_val.iter() {
                seq.serialize_element(&PyAnySerialize { obj: &item })?;
            }
            seq.end()
        } else if let Ok(tuple_val) = obj.downcast_exact::<PyTuple>() {
            let mut seq = serializer.serialize_seq(Some(tuple_val.len()))?;
            for item in tuple_val.iter() {
                seq.serialize_element(&PyAnySerialize { obj: &item })?;
            }
            seq.end()
        } else if let Ok(dict_val) = obj.downcast_exact::<PyDict>() {
            // Serialize Python dict to JSON object, only allowing string keys
            let mut map = serializer.serialize_map(Some(dict_val.len()))?;
            for (key, value) in dict_val.iter() {
                let key_str = key.extract::<String>().map_err(serde::ser::Error::custom)?;
                map.serialize_entry(&key_str, &PyAnySerialize { obj: &value })?;
            }
            map.end()
        } else {
            Err(serde::ser::Error::custom(format!(
                "Unsupported Python type for JSON serialization: {}",
                obj.get_type().name().and_then(|n| n.to_str().map(|s| s.to_owned())).unwrap_or_else(|_| "unknown".to_string())
            )))
        }
    }
}

/// Dumps a Python object into a JSON string.
///
/// Supported Python input types: dict, list, str, int, float, bool, and None.
/// Python tuples are serialized as JSON arrays. Dictionary keys must be strings.
///
/// # Arguments
/// * `py` - The Python GIL token.
/// * `data` - The Python object to serialize.
///
/// # Returns
/// A JSON string, or a PyValueError on error.
#[pyfunction]
fn dumps(_py: Python, data: &Bound<'_, PyAny>) -> PyResult<String> {
    serde_json::to_string(&PyAnySerialize { obj: data })
        .map_err(|e| PyValueError::new_err(format!("JSON serialization error: {e}")))
}

/// Python module definition for rjson.
///
/// Provides efficient JSON parsing (`loads`) and serialization (`dumps`) functions.
///
/// # Performance Note
/// This implementation is faster than Python's stdlib but slower than orjson due to
/// PyO3 and GIL overhead. For maximum speed, orjson uses raw buffer and SIMD techniques
/// not present here. This code prioritizes safety, maintainability, and idiomatic Rust.
#[pymodule]
fn rjson(_py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(loads, m)?)?;
    m.add_function(wrap_pyfunction!(dumps, m)?)?;
    // Add a function to convert a Python dict to a Rust struct.
    Ok(())
}
