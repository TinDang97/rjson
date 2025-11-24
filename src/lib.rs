use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use pyo3::types::{PyDict, PyList, PyBool, PyFloat, PyInt, PyString, PyTuple, PyAny};
use serde_json;
use serde::de::{self, Visitor, MapAccess, SeqAccess, Deserializer};
use serde::ser::{Serialize, Serializer, SerializeMap, SerializeSeq};
use std::fmt;
use serde::de::DeserializeSeed;

// Performance optimizations module
mod optimizations;
use optimizations::{object_cache, type_cache};
use type_cache::FastType;

#[allow(dead_code)]
/// Utility: Converts a Rust `serde_json::Value` into its corresponding Python object representation.
///
/// This function is not currently used, but is retained for potential future use or testing.
fn serde_value_to_py_object(py: Python, value: serde_json::Value) -> PyResult<PyObject> {
    match value {
        serde_json::Value::Null => Ok(object_cache::get_none(py)),
        serde_json::Value::Bool(b) => Ok(object_cache::get_bool(py, b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(object_cache::get_int(py, i))
            } else if let Some(u) = n.as_u64() {
                // For u64, check if it fits in i64 range for caching
                if u <= i64::MAX as u64 {
                    Ok(object_cache::get_int(py, u as i64))
                } else {
                    Ok(u.into_py(py))
                }
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_py(py))
            } else {
                Err(PyValueError::new_err(format!("Unsupported number value: {}", n)))
            }
        }
        serde_json::Value::String(s) => Ok(s.into_py(py)),
        serde_json::Value::Array(arr) => {
            let mut py_elements = Vec::with_capacity(arr.len());
            for item in arr {
                py_elements.push(serde_value_to_py_object(py, item)?);
            }
            let pylist = PyList::new(py, py_elements)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(pylist.to_object(py))
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, serde_value_to_py_object(py, v)?)?;
            }
            Ok(dict.to_object(py))
        }
    }
}

#[allow(dead_code)]
/// Utility: Converts a Python object (PyAny) to a serde_json::Value recursively.
///
/// This function is not currently used, but is retained for potential future use or testing.
fn py_object_to_serde_value(py: Python, obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    // Use fast type detection
    let fast_type = type_cache::get_fast_type(obj);

    match fast_type {
        FastType::None => Ok(serde_json::Value::Null),
        FastType::Bool => {
            let b_val = obj.downcast_exact::<PyBool>().unwrap();
            Ok(serde_json::Value::Bool(b_val.is_true()))
        }
        FastType::Int => {
            let l_val = obj.downcast_exact::<PyInt>().unwrap();
            if let Ok(val_i64) = l_val.extract::<i64>() {
                Ok(serde_json::Number::from(val_i64).into())
            } else if let Ok(val_u64) = l_val.extract::<u64>() {
                Ok(serde_json::Number::from(val_u64).into())
            } else if let Ok(val_f64) = l_val.extract::<f64>() {
                if val_f64.fract() == 0.0 {
                    if let Some(num) = serde_json::Number::from_f64(val_f64) {
                        return Ok(serde_json::Value::Number(num));
                    }
                }
                let s = l_val.to_string();
                match serde_json::from_str::<serde_json::Number>(&s) {
                    Ok(num) => Ok(serde_json::Value::Number(num)),
                    Err(_) => Err(PyValueError::new_err(format!(
                        "Could not convert Python integer {} to a JSON number", s
                    ))),
                }
            } else {
                let s = l_val.to_string();
                Err(PyValueError::new_err(format!(
                    "Could not convert Python integer {} to a JSON number", s
                )))
            }
        }
        FastType::Float => {
            let f_val = obj.downcast_exact::<PyFloat>().unwrap();
            let val_f64 = f_val.extract::<f64>()?;
            serde_json::Number::from_f64(val_f64)
                .map(serde_json::Value::Number)
                .ok_or_else(|| PyValueError::new_err(format!("Invalid Python float value for JSON: {} (e.g. NaN or Infinity)", val_f64)))
        }
        FastType::String => {
            let s_val = obj.downcast_exact::<PyString>().unwrap();
            Ok(serde_json::Value::String(s_val.to_str()?.to_owned()))
        }
        FastType::List => {
            let list_val = obj.downcast_exact::<PyList>().unwrap();
            let mut vec = Vec::with_capacity(list_val.len());
            for item_bound in list_val.iter() {
                vec.push(py_object_to_serde_value(py, &item_bound)?);
            }
            Ok(serde_json::Value::Array(vec))
        }
        FastType::Tuple => {
            let tuple_val = obj.downcast_exact::<PyTuple>().unwrap();
            let mut vec = Vec::with_capacity(tuple_val.len());
            for item_bound in tuple_val.iter() {
                vec.push(py_object_to_serde_value(py, &item_bound)?);
            }
            Ok(serde_json::Value::Array(vec))
        }
        FastType::Dict => {
            let dict_val = obj.downcast_exact::<PyDict>().unwrap();
            let mut map = serde_json::Map::new();
            for (key_bound, value_bound) in dict_val.iter() {
                let key_str = key_bound.extract::<String>()
                    .map_err(|e| PyValueError::new_err(format!("Dictionary keys must be strings for JSON serialization: {}", e)))?;
                map.insert(key_str, py_object_to_serde_value(py, &value_bound)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        FastType::Other => {
            Err(PyValueError::new_err(format!(
                "Unsupported Python type for JSON serialization: {}",
                obj.get_type().name()?
            )))
        }
    }
}

/// Optimized visitor that builds PyO3 objects directly from serde_json events.
///
/// Phase 1 Optimizations Applied:
/// - Integer caching for small values
/// - Pre-sized vector allocations with size hints
/// - Cached None/True/False singletons
struct PyObjectVisitor<'py> {
    py: Python<'py>,
}

impl<'de, 'py> Visitor<'de> for PyObjectVisitor<'py> {
    type Value = PyObject;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    #[inline]
    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        // OPTIMIZATION: Use cached boolean singletons
        Ok(object_cache::get_bool(self.py, v))
    }

    #[inline]
    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        // OPTIMIZATION: Inline cache check to avoid function call overhead
        // Only use cache for small values where it's beneficial
        if v >= -256 && v <= 256 {
            Ok(object_cache::get_int(self.py, v))
        } else {
            // Fast path: direct conversion for large integers
            Ok(v.to_object(self.py))
        }
    }

    #[inline]
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        // OPTIMIZATION: Only cache if value fits in small integer range
        if v <= 256 {
            Ok(object_cache::get_int(self.py, v as i64))
        } else {
            Ok(v.to_object(self.py))
        }
    }

    #[inline]
    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }

    #[inline]
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }

    #[inline]
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }

    #[inline]
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        // OPTIMIZATION: Use cached None singleton
        Ok(object_cache::get_none(self.py))
    }

    #[inline]
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        // OPTIMIZATION: Use cached None singleton
        Ok(object_cache::get_none(self.py))
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
        // OPTIMIZATION Phase 1.3: Pre-allocate with size hint
        let size = seq.size_hint().unwrap_or(0);
        let mut elements = Vec::with_capacity(size);

        // Collect all elements
        while let Some(elem) = seq.next_element_seed(PyObjectSeed { py: self.py })? {
            elements.push(elem);
        }

        use serde::de::Error as SerdeDeError;
        let pylist = PyList::new(self.py, &elements)
            .map_err(|e| SerdeDeError::custom(e.to_string()))?;
        Ok(pylist.to_object(self.py))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // OPTIMIZATION Phase 1.5: Direct dict insertion without intermediate Vecs
        // This eliminates 2 heap allocations and improves cache locality
        use serde::de::Error as SerdeDeError;

        let dict = PyDict::new(self.py);

        // Insert directly into dict as we parse
        while let Some((key, value)) = map.next_entry_seed(KeySeed, PyObjectSeed { py: self.py })? {
            dict.set_item(&key, &value)
                .map_err(|e| SerdeDeError::custom(format!("Failed to insert into dict: {}", e)))?;
        }

        Ok(dict.to_object(self.py))
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
/// Phase 1 Optimizations: Uses integer caching and optimized type detection.
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
        DeserializeSeed::deserialize(PyObjectSeed { py }, &mut de)
            .map_err(|e| PyValueError::new_err(format!("JSON parsing error: {e}")))
    })
}

/// Optimized wrapper to implement serde::Serialize for PyAny (Python objects).
///
/// Phase 1 Optimizations Applied:
/// - Fast type detection using cached type pointers
/// - Eliminates sequential if-else downcast chain
struct PyAnySerialize<'py> {
    obj: &'py Bound<'py, PyAny>,
}

impl<'py> Serialize for PyAnySerialize<'py> {
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let obj = self.obj;

        // OPTIMIZATION Phase 1.2: Use fast type detection instead of sequential downcasts
        let fast_type = type_cache::get_fast_type(obj);

        match fast_type {
            FastType::None => serializer.serialize_unit(),

            FastType::Bool => {
                let b_val = obj.downcast_exact::<PyBool>().unwrap();
                serializer.serialize_bool(b_val.is_true())
            }

            FastType::Int => {
                let l_val = obj.downcast_exact::<PyInt>().unwrap();
                if let Ok(val_i64) = l_val.extract::<i64>() {
                    serializer.serialize_i64(val_i64)
                } else if let Ok(val_u64) = l_val.extract::<u64>() {
                    serializer.serialize_u64(val_u64)
                } else if let Ok(val_f64) = l_val.extract::<f64>() {
                    if val_f64.fract() == 0.0 {
                        serializer.serialize_f64(val_f64)
                    } else {
                        Err(serde::ser::Error::custom("Cannot serialize non-integer PyInt as JSON number"))
                    }
                } else {
                    let s = l_val.to_string();
                    serializer.serialize_str(&s)
                }
            }

            FastType::Float => {
                let f_val = obj.downcast_exact::<PyFloat>().unwrap();
                let val_f64 = f_val.extract::<f64>().map_err(serde::ser::Error::custom)?;
                serializer.serialize_f64(val_f64)
            }

            FastType::String => {
                let s_val = obj.downcast_exact::<PyString>().unwrap();
                serializer.serialize_str(s_val.to_str().map_err(serde::ser::Error::custom)?)
            }

            FastType::List => {
                let list_val = obj.downcast_exact::<PyList>().unwrap();
                let mut seq = serializer.serialize_seq(Some(list_val.len()))?;
                for item in list_val.iter() {
                    seq.serialize_element(&PyAnySerialize { obj: &item })?;
                }
                seq.end()
            }

            FastType::Tuple => {
                let tuple_val = obj.downcast_exact::<PyTuple>().unwrap();
                let mut seq = serializer.serialize_seq(Some(tuple_val.len()))?;
                for item in tuple_val.iter() {
                    seq.serialize_element(&PyAnySerialize { obj: &item })?;
                }
                seq.end()
            }

            FastType::Dict => {
                let dict_val = obj.downcast_exact::<PyDict>().unwrap();
                let mut map = serializer.serialize_map(Some(dict_val.len()))?;
                for (key, value) in dict_val.iter() {
                    let key_str = key.extract::<String>().map_err(serde::ser::Error::custom)?;
                    map.serialize_entry(&key_str, &PyAnySerialize { obj: &value })?;
                }
                map.end()
            }

            FastType::Other => {
                Err(serde::ser::Error::custom(format!(
                    "Unsupported Python type for JSON serialization: {}",
                    obj.get_type().name().and_then(|n| n.to_str().map(|s| s.to_owned())).unwrap_or_else(|_| "unknown".to_string())
                )))
            }
        }
    }
}

/// Dumps a Python object into a JSON string.
///
/// Phase 1 Optimizations: Uses fast type detection for improved serialization performance.
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
/// Provides optimized JSON parsing (`loads`) and serialization (`dumps`) functions.
///
/// # Performance Optimizations (Phase 1)
/// - Integer caching for values [-256, 256]
/// - Boolean and None singleton caching
/// - Fast type detection using cached type pointers
/// - Pre-sized vector allocations
///
/// Expected speedup: 20-30% over baseline implementation
#[pymodule]
fn rjson(py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // OPTIMIZATION: Initialize caches at module load time
    object_cache::init_cache(py);
    type_cache::init_type_cache(py);

    m.add_function(wrap_pyfunction!(loads, m)?)?;
    m.add_function(wrap_pyfunction!(dumps, m)?)?;
    Ok(())
}
