use bstr::ByteSlice;
use num_bigint::{BigInt, ToBigInt};
use num_integer::Integer;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::convert::TryFrom;
use std::ops::Range;

use super::objbytearray::{PyByteArray, PyByteArrayRef};
use super::objbytes::{PyBytes, PyBytesRef};
use super::objint::{self, PyInt, PyIntRef};
use super::objlist::PyList;
use super::objmemory::PyMemoryView;
use super::objnone::PyNoneRef;
use super::objsequence::{is_valid_slice_arg, PySliceableSequence};
use super::objslice::PySliceRef;
use super::objstr::{self, adjust_indices, PyString, PyStringRef, StringRange};
use super::objtuple::PyTupleRef;
use crate::function::{OptionalArg, OptionalOption};
use crate::pyhash;
use crate::pyobject::{
    Either, PyComparisonValue, PyIterable, PyObjectRef, PyResult, ThreadSafe, TryFromObject,
    TypeProtocol,
};
use crate::vm::VirtualMachine;

#[derive(Debug, Default, Clone)]
pub struct PyByteInner {
    pub elements: Vec<u8>,
}

impl ThreadSafe for PyByteInner {}

impl TryFromObject for PyByteInner {
    fn try_from_object(vm: &VirtualMachine, obj: PyObjectRef) -> PyResult<Self> {
        match_class!(match obj {
            i @ PyBytes => Ok(PyByteInner {
                elements: i.get_value().to_vec()
            }),
            j @ PyByteArray => Ok(PyByteInner {
                elements: j.borrow_value().elements.to_vec()
            }),
            k @ PyMemoryView => Ok(PyByteInner {
                elements: k.try_value().unwrap()
            }),
            l @ PyList => l.get_byte_inner(vm),
            obj => {
                let iter = vm.get_method_or_type_error(obj.clone(), "__iter__", || {
                    format!("a bytes-like object is required, not {}", obj.class())
                })?;
                let iter = PyIterable::from_method(iter);
                Ok(PyByteInner {
                    elements: iter.iter(vm)?.collect::<PyResult<_>>()?,
                })
            }
        })
    }
}

#[derive(FromArgs)]
pub struct ByteInnerNewOptions {
    #[pyarg(positional_only, optional = true)]
    val_option: OptionalArg<PyObjectRef>,
    #[pyarg(positional_or_keyword, optional = true)]
    encoding: OptionalArg<PyStringRef>,
}

impl ByteInnerNewOptions {
    pub fn get_value(self, vm: &VirtualMachine) -> PyResult<PyByteInner> {
        // First handle bytes(string, encoding[, errors])
        if let OptionalArg::Present(enc) = self.encoding {
            if let OptionalArg::Present(eval) = self.val_option {
                if let Ok(input) = eval.downcast::<PyString>() {
                    let bytes = objstr::encode_string(input, Some(enc), None, vm)?;
                    Ok(PyByteInner {
                        elements: bytes.get_value().to_vec(),
                    })
                } else {
                    Err(vm.new_type_error("encoding without a string argument".to_owned()))
                }
            } else {
                Err(vm.new_type_error("encoding without a string argument".to_owned()))
            }
        // Only one argument
        } else {
            let value = if let OptionalArg::Present(ival) = self.val_option {
                match_class!(match ival.clone() {
                    i @ PyInt => {
                        let size =
                            objint::get_value(&i.into_object())
                                .to_isize()
                                .ok_or_else(|| {
                                    vm.new_overflow_error(
                                        "cannot fit 'int' into an index-sized integer".to_owned(),
                                    )
                                })?;
                        let size = if size < 0 {
                            return Err(vm.new_value_error("negative count".to_owned()));
                        } else {
                            size as usize
                        };
                        Ok(vec![0; size])
                    }
                    _l @ PyString => {
                        return Err(
                            vm.new_type_error("string argument without an encoding".to_owned())
                        );
                    }
                    i @ PyBytes => Ok(i.get_value().to_vec()),
                    j @ PyByteArray => Ok(j.borrow_value().elements.to_vec()),
                    obj => {
                        // TODO: only support this method in the bytes() constructor
                        if let Some(bytes_method) = vm.get_method(obj.clone(), "__bytes__") {
                            let bytes = vm.invoke(&bytes_method?, vec![])?;
                            return PyByteInner::try_from_object(vm, bytes);
                        }
                        let elements = vm.extract_elements(&obj).or_else(|_| {
                            Err(vm.new_type_error(format!(
                                "cannot convert '{}' object to bytes",
                                obj.class().name
                            )))
                        })?;

                        let mut data_bytes = vec![];
                        for elem in elements {
                            let v = objint::to_int(vm, &elem, &BigInt::from(10))?;
                            if let Some(i) = v.to_u8() {
                                data_bytes.push(i);
                            } else {
                                return Err(
                                    vm.new_value_error("bytes must be in range(0, 256)".to_owned())
                                );
                            }
                        }
                        Ok(data_bytes)
                    }
                })
            } else {
                Ok(vec![])
            };
            match value {
                Ok(val) => Ok(PyByteInner { elements: val }),
                Err(err) => Err(err),
            }
        }
    }
}

#[derive(FromArgs)]
pub struct ByteInnerFindOptions {
    #[pyarg(positional_only, optional = false)]
    sub: Either<PyByteInner, PyIntRef>,
    #[pyarg(positional_only, optional = true)]
    start: OptionalArg<Option<isize>>,
    #[pyarg(positional_only, optional = true)]
    end: OptionalArg<Option<isize>>,
}

impl ByteInnerFindOptions {
    pub fn get_value(
        self,
        len: usize,
        vm: &VirtualMachine,
    ) -> PyResult<(Vec<u8>, std::ops::Range<usize>)> {
        let sub = match self.sub {
            Either::A(v) => v.elements.to_vec(),
            Either::B(int) => vec![int.as_bigint().byte_or(vm)?],
        };
        let range = adjust_indices(self.start, self.end, len);
        Ok((sub, range))
    }
}

#[derive(FromArgs)]
pub struct ByteInnerPaddingOptions {
    #[pyarg(positional_only, optional = false)]
    width: PyIntRef,
    #[pyarg(positional_only, optional = true)]
    fillbyte: OptionalArg<PyObjectRef>,
}
impl ByteInnerPaddingOptions {
    fn get_value(self, fn_name: &str, len: usize, vm: &VirtualMachine) -> PyResult<(u8, usize)> {
        let fillbyte = if let OptionalArg::Present(v) = &self.fillbyte {
            match try_as_byte(&v) {
                Some(x) => {
                    if x.len() == 1 {
                        x[0]
                    } else {
                        return Err(vm.new_type_error(format!(
                            "{}() argument 2 must be a byte string of length 1, not {}",
                            fn_name, &v
                        )));
                    }
                }
                None => {
                    return Err(vm.new_type_error(format!(
                        "{}() argument 2 must be a byte string of length 1, not {}",
                        fn_name, &v
                    )));
                }
            }
        } else {
            b' ' // default is space
        };

        // <0 = no change
        let width = if let Some(x) = self.width.as_bigint().to_usize() {
            if x <= len {
                0
            } else {
                x
            }
        } else {
            0
        };

        let diff: usize = if width != 0 { width - len } else { 0 };

        Ok((fillbyte, diff))
    }
}

#[derive(FromArgs)]
pub struct ByteInnerTranslateOptions {
    #[pyarg(positional_only, optional = false)]
    table: Either<PyByteInner, PyNoneRef>,
    #[pyarg(positional_or_keyword, optional = true)]
    delete: OptionalArg<PyByteInner>,
}

impl ByteInnerTranslateOptions {
    pub fn get_value(self, vm: &VirtualMachine) -> PyResult<(Vec<u8>, Vec<u8>)> {
        let table = match self.table {
            Either::A(v) => v.elements.to_vec(),
            Either::B(_) => (0..=255).collect::<Vec<u8>>(),
        };

        if table.len() != 256 {
            return Err(
                vm.new_value_error("translation table must be 256 characters long".to_owned())
            );
        }

        let delete = match self.delete {
            OptionalArg::Present(byte) => byte.elements,
            _ => vec![],
        };

        Ok((table, delete))
    }
}

#[derive(FromArgs)]
pub struct ByteInnerSplitOptions {
    #[pyarg(positional_or_keyword, optional = true)]
    sep: OptionalArg<Option<PyByteInner>>,
    #[pyarg(positional_or_keyword, optional = true)]
    maxsplit: OptionalArg<isize>,
}

impl ByteInnerSplitOptions {
    pub fn get_value(self) -> PyResult<(Vec<u8>, isize)> {
        let sep = match self.sep.into_option() {
            Some(Some(bytes)) => bytes.elements,
            _ => vec![],
        };

        let maxsplit = if let OptionalArg::Present(value) = self.maxsplit {
            value
        } else {
            -1
        };

        Ok((sep, maxsplit))
    }
}

#[derive(FromArgs)]
pub struct ByteInnerExpandtabsOptions {
    #[pyarg(positional_or_keyword, optional = true)]
    tabsize: OptionalArg<PyIntRef>,
}

impl ByteInnerExpandtabsOptions {
    pub fn get_value(self) -> usize {
        match self.tabsize.into_option() {
            Some(int) => int.as_bigint().to_usize().unwrap_or(0),
            None => 8,
        }
    }
}

#[derive(FromArgs)]
pub struct ByteInnerSplitlinesOptions {
    #[pyarg(positional_or_keyword, optional = true)]
    keepends: OptionalArg<bool>,
}

impl ByteInnerSplitlinesOptions {
    pub fn get_value(self) -> bool {
        match self.keepends.into_option() {
            Some(x) => x,
            None => false,
        }
        // if let OptionalArg::Present(value) = self.keepends {
        //     Ok(bool::try_from_object(vm, value)?)
        // } else {
        //     Ok(false)
        // }
    }
}

#[allow(clippy::len_without_is_empty)]
impl PyByteInner {
    pub fn repr(&self) -> PyResult<String> {
        let mut res = String::with_capacity(self.elements.len());
        for i in self.elements.iter() {
            match i {
                0..=8 => res.push_str(&format!("\\x0{}", i)),
                9 => res.push_str("\\t"),
                10 => res.push_str("\\n"),
                11 => res.push_str(&format!("\\x0{:x}", i)),
                13 => res.push_str("\\r"),
                32..=126 => res.push(*(i) as char),
                _ => res.push_str(&format!("\\x{:x}", i)),
            }
        }
        Ok(res)
    }

    pub fn len(&self) -> usize {
        self.elements.len()
    }

    #[inline]
    fn cmp<F>(&self, other: PyObjectRef, op: F, vm: &VirtualMachine) -> PyComparisonValue
    where
        F: Fn(&[u8], &[u8]) -> bool,
    {
        let r = PyBytesLike::try_from_object(vm, other)
            .map(|other| other.with_ref(|other| op(&self.elements, other)));
        PyComparisonValue::from_option(r.ok())
    }

    pub fn eq(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyComparisonValue {
        self.cmp(other, |a, b| a == b, vm)
    }

    pub fn ge(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyComparisonValue {
        self.cmp(other, |a, b| a >= b, vm)
    }

    pub fn le(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyComparisonValue {
        self.cmp(other, |a, b| a <= b, vm)
    }

    pub fn gt(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyComparisonValue {
        self.cmp(other, |a, b| a > b, vm)
    }

    pub fn lt(&self, other: PyObjectRef, vm: &VirtualMachine) -> PyComparisonValue {
        self.cmp(other, |a, b| a < b, vm)
    }

    pub fn hash(&self) -> pyhash::PyHash {
        pyhash::hash_value(&self.elements)
    }

    pub fn add(&self, other: PyByteInner) -> Vec<u8> {
        self.elements
            .iter()
            .chain(other.elements.iter())
            .cloned()
            .collect::<Vec<u8>>()
    }

    pub fn contains(
        &self,
        needle: Either<PyByteInner, PyIntRef>,
        vm: &VirtualMachine,
    ) -> PyResult<bool> {
        match needle {
            Either::A(byte) => {
                if byte.elements.is_empty() {
                    return Ok(true);
                }
                let other = &byte.elements[..];
                for (n, i) in self.elements.iter().enumerate() {
                    if n + other.len() <= self.len()
                        && *i == other[0]
                        && &self.elements[n..n + other.len()] == other
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Either::B(int) => Ok(self.elements.contains(&int.as_bigint().byte_or(vm)?)),
        }
    }

    pub fn getitem(&self, needle: Either<i32, PySliceRef>, vm: &VirtualMachine) -> PyResult {
        match needle {
            Either::A(int) => {
                if let Some(idx) = self.elements.get_pos(int) {
                    Ok(vm.new_int(self.elements[idx]))
                } else {
                    Err(vm.new_index_error("index out of range".to_owned()))
                }
            }
            Either::B(slice) => Ok(vm
                .ctx
                .new_bytes(self.elements.get_slice_items(vm, slice.as_object())?)),
        }
    }

    fn setindex(&mut self, int: i32, object: PyObjectRef, vm: &VirtualMachine) -> PyResult {
        if let Some(idx) = self.elements.get_pos(int) {
            let result = match_class!(match object {
                i @ PyInt => {
                    if let Some(value) = i.as_bigint().to_u8() {
                        Ok(value)
                    } else {
                        Err(vm.new_value_error("byte must be in range(0, 256)".to_owned()))
                    }
                }
                _ => Err(vm.new_type_error("an integer is required".to_owned())),
            });
            let value = result?;
            self.elements[idx] = value;
            Ok(vm.new_int(value))
        } else {
            Err(vm.new_index_error("index out of range".to_owned()))
        }
    }

    fn setslice(
        &mut self,
        slice: PySliceRef,
        object: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult {
        let sec = match PyIterable::try_from_object(vm, object.clone()) {
            Ok(sec) => {
                let items: Result<Vec<PyObjectRef>, _> = sec.iter(vm)?.collect();
                Ok(items?
                    .into_iter()
                    .map(|obj| u8::try_from_object(vm, obj))
                    .collect::<PyResult<Vec<_>>>()?)
            }
            _ => match_class!(match object {
                i @ PyMemoryView => Ok(i.try_value().unwrap()),
                _ => Err(vm.new_index_error(
                    "can assign only bytes, buffers, or iterables of ints in range(0, 256)"
                        .to_owned()
                )),
            }),
        };
        let items = sec?;
        let range = self
            .elements
            .get_slice_range(&slice.start_index(vm)?, &slice.stop_index(vm)?);
        self.elements.splice(range, items);
        Ok(vm
            .ctx
            .new_bytes(self.elements.get_slice_items(vm, slice.as_object())?))
    }

    pub fn setitem(
        &mut self,
        needle: Either<i32, PySliceRef>,
        object: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult {
        match needle {
            Either::A(int) => self.setindex(int, object, vm),
            Either::B(slice) => self.setslice(slice, object, vm),
        }
    }

    pub fn delitem(
        &mut self,
        needle: Either<i32, PySliceRef>,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        match needle {
            Either::A(int) => {
                if let Some(idx) = self.elements.get_pos(int) {
                    self.elements.remove(idx);
                    Ok(())
                } else {
                    Err(vm.new_index_error("index out of range".to_owned()))
                }
            }
            Either::B(slice) => self.delslice(slice, vm),
        }
    }

    // TODO: deduplicate this with the code in objlist
    fn delslice(&mut self, slice: PySliceRef, vm: &VirtualMachine) -> PyResult<()> {
        let start = slice.start_index(vm)?;
        let stop = slice.stop_index(vm)?;
        let step = slice.step_index(vm)?.unwrap_or_else(BigInt::one);

        if step.is_zero() {
            Err(vm.new_value_error("slice step cannot be zero".to_owned()))
        } else if step.is_positive() {
            let range = self.elements.get_slice_range(&start, &stop);
            if range.start < range.end {
                #[allow(clippy::range_plus_one)]
                match step.to_i32() {
                    Some(1) => {
                        self._del_slice(range);
                        Ok(())
                    }
                    Some(num) => {
                        self._del_stepped_slice(range, num as usize);
                        Ok(())
                    }
                    None => {
                        self._del_slice(range.start..range.start + 1);
                        Ok(())
                    }
                }
            } else {
                // no del to do
                Ok(())
            }
        } else {
            // calculate the range for the reverse slice, first the bounds needs to be made
            // exclusive around stop, the lower number
            let start = start.as_ref().map(|x| {
                if *x == (-1).to_bigint().unwrap() {
                    self.elements.len() + BigInt::one() //.to_bigint().unwrap()
                } else {
                    x + 1
                }
            });
            let stop = stop.as_ref().map(|x| {
                if *x == (-1).to_bigint().unwrap() {
                    self.elements.len().to_bigint().unwrap()
                } else {
                    x + 1
                }
            });
            let range = self.elements.get_slice_range(&stop, &start);
            if range.start < range.end {
                match (-step).to_i32() {
                    Some(1) => {
                        self._del_slice(range);
                        Ok(())
                    }
                    Some(num) => {
                        self._del_stepped_slice_reverse(range, num as usize);
                        Ok(())
                    }
                    None => {
                        self._del_slice(range.end - 1..range.end);
                        Ok(())
                    }
                }
            } else {
                // no del to do
                Ok(())
            }
        }
    }

    fn _del_slice(&mut self, range: Range<usize>) {
        self.elements.drain(range);
    }

    fn _del_stepped_slice(&mut self, range: Range<usize>, step: usize) {
        // no easy way to delete stepped indexes so here is what we'll do
        let mut deleted = 0;
        let elements = &mut self.elements;
        let mut indexes = range.clone().step_by(step).peekable();

        for i in range.clone() {
            // is this an index to delete?
            if indexes.peek() == Some(&i) {
                // record and move on
                indexes.next();
                deleted += 1;
            } else {
                // swap towards front
                elements.swap(i - deleted, i);
            }
        }
        // then drain (the values to delete should now be contiguous at the end of the range)
        elements.drain((range.end - deleted)..range.end);
    }

    fn _del_stepped_slice_reverse(&mut self, range: Range<usize>, step: usize) {
        // no easy way to delete stepped indexes so here is what we'll do
        let mut deleted = 0;
        let elements = &mut self.elements;
        let mut indexes = range.clone().rev().step_by(step).peekable();

        for i in range.clone().rev() {
            // is this an index to delete?
            if indexes.peek() == Some(&i) {
                // record and move on
                indexes.next();
                deleted += 1;
            } else {
                // swap towards back
                elements.swap(i + deleted, i);
            }
        }
        // then drain (the values to delete should now be contiguous at teh start of the range)
        elements.drain(range.start..(range.start + deleted));
    }

    pub fn isalnum(&self) -> bool {
        !self.elements.is_empty()
            && self
                .elements
                .iter()
                .all(|x| char::from(*x).is_alphanumeric())
    }

    pub fn isalpha(&self) -> bool {
        !self.elements.is_empty() && self.elements.iter().all(|x| char::from(*x).is_alphabetic())
    }

    pub fn isascii(&self) -> bool {
        self.elements.iter().all(|x| char::from(*x).is_ascii())
    }

    pub fn isdigit(&self) -> bool {
        !self.elements.is_empty() && self.elements.iter().all(|x| char::from(*x).is_digit(10))
    }

    pub fn islower(&self) -> bool {
        // CPython _Py_bytes_islower
        let mut cased = false;
        for b in self.elements.iter() {
            let c = *b as char;
            if c.is_uppercase() {
                return false;
            } else if !cased && c.is_lowercase() {
                cased = true
            }
        }
        cased
    }

    pub fn isupper(&self) -> bool {
        // CPython _Py_bytes_isupper
        let mut cased = false;
        for b in self.elements.iter() {
            let c = *b as char;
            if c.is_lowercase() {
                return false;
            } else if !cased && c.is_uppercase() {
                cased = true
            }
        }
        cased
    }

    pub fn isspace(&self) -> bool {
        !self.elements.is_empty()
            && self
                .elements
                .iter()
                .all(|x| char::from(*x).is_ascii_whitespace())
    }

    pub fn istitle(&self) -> bool {
        if self.elements.is_empty() {
            return false;
        }

        let mut iter = self.elements.iter().peekable();
        let mut prev_cased = false;

        while let Some(c) = iter.next() {
            let current = char::from(*c);
            let next = if let Some(k) = iter.peek() {
                char::from(**k)
            } else if current.is_uppercase() {
                return !prev_cased;
            } else {
                return prev_cased;
            };

            let is_cased = current.to_uppercase().next().unwrap() != current
                || current.to_lowercase().next().unwrap() != current;
            if (is_cased && next.is_uppercase() && !prev_cased)
                || (!is_cased && next.is_lowercase())
            {
                return false;
            }

            prev_cased = is_cased;
        }

        true
    }

    pub fn lower(&self) -> Vec<u8> {
        self.elements.to_ascii_lowercase()
    }

    pub fn upper(&self) -> Vec<u8> {
        self.elements.to_ascii_uppercase()
    }

    pub fn capitalize(&self) -> Vec<u8> {
        let mut new: Vec<u8> = Vec::with_capacity(self.elements.len());
        if let Some((first, second)) = self.elements.split_first() {
            new.push(first.to_ascii_uppercase());
            second.iter().for_each(|x| new.push(x.to_ascii_lowercase()));
        }
        new
    }

    pub fn swapcase(&self) -> Vec<u8> {
        let mut new: Vec<u8> = Vec::with_capacity(self.elements.len());
        for w in &self.elements {
            match w {
                65..=90 => new.push(w.to_ascii_lowercase()),
                97..=122 => new.push(w.to_ascii_uppercase()),
                x => new.push(*x),
            }
        }
        new
    }

    pub fn hex(&self) -> String {
        self.elements
            .iter()
            .map(|x| format!("{:02x}", x))
            .collect::<String>()
    }

    pub fn fromhex(string: &str, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
        // first check for invalid character
        for (i, c) in string.char_indices() {
            if !c.is_digit(16) && !c.is_whitespace() {
                return Err(vm.new_value_error(format!(
                    "non-hexadecimal number found in fromhex() arg at position {}",
                    i
                )));
            }
        }

        // strip white spaces
        let stripped = string.split_whitespace().collect::<String>();

        // Hex is evaluated on 2 digits
        if stripped.len() % 2 != 0 {
            return Err(vm.new_value_error(format!(
                "non-hexadecimal number found in fromhex() arg at position {}",
                stripped.len() - 1
            )));
        }

        // parse even string
        Ok(stripped
            .chars()
            .collect::<Vec<char>>()
            .chunks(2)
            .map(|x| x.to_vec().iter().collect::<String>())
            .map(|x| u8::from_str_radix(&x, 16).unwrap())
            .collect::<Vec<u8>>())
    }

    pub fn center(
        &self,
        options: ByteInnerPaddingOptions,
        vm: &VirtualMachine,
    ) -> PyResult<Vec<u8>> {
        let (fillbyte, diff) = options.get_value("center", self.len(), vm)?;

        let mut ln: usize = diff / 2;
        let mut rn: usize = ln;

        if diff.is_odd() && self.len() % 2 == 0 {
            ln += 1
        }

        if diff.is_odd() && self.len() % 2 != 0 {
            rn += 1
        }

        // merge all
        let mut res = vec![fillbyte; ln];
        res.extend_from_slice(&self.elements[..]);
        res.extend_from_slice(&vec![fillbyte; rn][..]);

        Ok(res)
    }

    pub fn ljust(
        &self,
        options: ByteInnerPaddingOptions,
        vm: &VirtualMachine,
    ) -> PyResult<Vec<u8>> {
        let (fillbyte, diff) = options.get_value("ljust", self.len(), vm)?;

        // merge all
        let mut res = vec![];
        res.extend_from_slice(&self.elements[..]);
        res.extend_from_slice(&vec![fillbyte; diff][..]);

        Ok(res)
    }

    pub fn rjust(
        &self,
        options: ByteInnerPaddingOptions,
        vm: &VirtualMachine,
    ) -> PyResult<Vec<u8>> {
        let (fillbyte, diff) = options.get_value("rjust", self.len(), vm)?;

        // merge all
        let mut res = vec![fillbyte; diff];
        res.extend_from_slice(&self.elements[..]);

        Ok(res)
    }

    pub fn count(&self, options: ByteInnerFindOptions, vm: &VirtualMachine) -> PyResult<usize> {
        let (needle, range) = options.get_value(self.elements.len(), vm)?;
        if !range.is_normal() {
            return Ok(0);
        }
        if needle.is_empty() {
            return Ok(range.len() + 1);
        }
        let haystack = &self.elements[range];
        let total = haystack
            .windows(needle.len())
            .filter(|w| *w == needle.as_slice())
            .count();
        Ok(total)
    }

    pub fn join(&self, iter: PyIterable<PyByteInner>, vm: &VirtualMachine) -> PyResult<Vec<u8>> {
        let mut refs = Vec::new();
        for v in iter.iter(vm)? {
            let v = v?;
            if !refs.is_empty() {
                refs.extend(&self.elements);
            }
            refs.extend(v.elements);
        }

        Ok(refs)
    }

    #[inline]
    pub fn startsendswith(
        &self,
        arg: Either<PyByteInner, PyTupleRef>,
        start: OptionalArg<PyObjectRef>,
        end: OptionalArg<PyObjectRef>,
        endswith: bool, // true for endswith, false for startswith
        vm: &VirtualMachine,
    ) -> PyResult<bool> {
        let suff = match arg {
            Either::A(byte) => byte.elements,
            Either::B(tuple) => {
                let mut flatten = vec![];
                for v in tuple.as_slice() {
                    flatten.extend(PyByteInner::try_from_object(vm, v.clone())?.elements)
                }
                flatten
            }
        };

        if suff.is_empty() {
            return Ok(true);
        }
        let range = self.elements.get_slice_range(
            &is_valid_slice_arg(start, vm)?,
            &is_valid_slice_arg(end, vm)?,
        );

        if range.end - range.start < suff.len() {
            return Ok(false);
        }

        let offset = if endswith {
            (range.end - suff.len())..range.end
        } else {
            0..suff.len()
        };

        Ok(suff.as_slice() == &self.elements.do_slice(range)[offset])
    }

    #[inline]
    pub fn find(
        &self,
        options: ByteInnerFindOptions,
        reverse: bool,
        vm: &VirtualMachine,
    ) -> PyResult<Option<usize>> {
        let (needle, range) = options.get_value(self.elements.len(), vm)?;
        if !range.is_normal() {
            return Ok(None);
        }
        if needle.is_empty() {
            return Ok(Some(if reverse { range.end } else { range.start }));
        }
        let haystack = &self.elements[range.clone()];
        let windows = haystack.windows(needle.len());
        if reverse {
            for (i, w) in windows.rev().enumerate() {
                if w == needle.as_slice() {
                    return Ok(Some(range.end - i - needle.len()));
                }
            }
        } else {
            for (i, w) in windows.enumerate() {
                if w == needle.as_slice() {
                    return Ok(Some(range.start + i));
                }
            }
        }
        Ok(None)
    }

    pub fn maketrans(from: PyByteInner, to: PyByteInner, vm: &VirtualMachine) -> PyResult {
        let mut res = vec![];

        for i in 0..=255 {
            res.push(
                if let Some(position) = from.elements.iter().position(|&x| x == i) {
                    to.elements[position]
                } else {
                    i
                },
            );
        }

        Ok(vm.ctx.new_bytes(res))
    }

    pub fn translate(
        &self,
        options: ByteInnerTranslateOptions,
        vm: &VirtualMachine,
    ) -> PyResult<Vec<u8>> {
        let (table, delete) = options.get_value(vm)?;

        let mut res = if delete.is_empty() {
            Vec::with_capacity(self.elements.len())
        } else {
            Vec::new()
        };

        for i in self.elements.iter() {
            if !delete.contains(&i) {
                res.push(table[*i as usize]);
            }
        }

        Ok(res)
    }

    pub fn strip(&self, chars: OptionalOption<PyByteInner>) -> Vec<u8> {
        let chars = chars.flat_option();
        let chars = match chars {
            Some(ref chars) => &chars.elements,
            None => return self.elements.trim().to_owned(),
        };
        self.elements
            .trim_with(|c| chars.contains(&(c as u8)))
            .to_owned()
    }

    pub fn lstrip(&self, chars: OptionalOption<PyByteInner>) -> Vec<u8> {
        let chars = chars.flat_option();
        let chars = match chars {
            Some(ref chars) => &chars.elements,
            None => return self.elements.trim_start().to_owned(),
        };
        self.elements
            .trim_start_with(|c| chars.contains(&(c as u8)))
            .to_owned()
    }

    pub fn rstrip(&self, chars: OptionalOption<PyByteInner>) -> Vec<u8> {
        let chars = chars.flat_option();
        let chars = match chars {
            Some(ref chars) => &chars.elements,
            None => return self.elements.trim_end().to_owned(),
        };
        self.elements
            .trim_end_with(|c| chars.contains(&(c as u8)))
            .to_owned()
    }

    pub fn split(&self, options: ByteInnerSplitOptions, reverse: bool) -> PyResult<Vec<&[u8]>> {
        let (sep, maxsplit) = options.get_value()?;

        if self.elements.is_empty() {
            if !sep.is_empty() {
                return Ok(vec![&[]]);
            }
            return Ok(vec![]);
        }

        if reverse {
            Ok(split_slice_reverse(&self.elements, &sep, maxsplit))
        } else {
            Ok(split_slice(&self.elements, &sep, maxsplit))
        }
    }

    pub fn partition(
        &self,
        sub: &PyByteInner,
        vm: &VirtualMachine,
    ) -> PyResult<(Vec<u8>, bool, Vec<u8>)> {
        if sub.elements.is_empty() {
            return Err(vm.new_value_error("empty separator".to_owned()));
        }

        let mut sp = self.elements.splitn_str(2, &sub.elements);
        let front = sp.next().unwrap().to_vec();
        let (has_mid, back) = if let Some(back) = sp.next() {
            (true, back.to_vec())
        } else {
            (false, Vec::new())
        };
        Ok((front, has_mid, back))
    }

    pub fn rpartition(
        &self,
        sub: &PyByteInner,
        vm: &VirtualMachine,
    ) -> PyResult<(Vec<u8>, bool, Vec<u8>)> {
        if sub.elements.is_empty() {
            return Err(vm.new_value_error("empty separator".to_owned()));
        }

        let mut sp = self.elements.rsplitn_str(2, &sub.elements);
        let back = sp.next().unwrap().to_vec();
        let (has_mid, front) = if let Some(front) = sp.next() {
            (true, front.to_vec())
        } else {
            (false, Vec::new())
        };
        Ok((front, has_mid, back))
    }

    pub fn expandtabs(&self, options: ByteInnerExpandtabsOptions) -> Vec<u8> {
        let tabsize = options.get_value();
        let mut counter: usize = 0;
        let mut res = vec![];

        if tabsize == 0 {
            return self
                .elements
                .iter()
                .cloned()
                .filter(|x| *x != b'\t')
                .collect::<Vec<u8>>();
        }

        for i in &self.elements {
            if *i == b'\t' {
                let len = tabsize - counter % tabsize;
                res.extend_from_slice(&vec![b' '; len]);
                counter += len;
            } else {
                res.push(*i);
                if *i == b'\r' || *i == b'\n' {
                    counter = 0;
                } else {
                    counter += 1;
                }
            }
        }

        res
    }

    pub fn splitlines(&self, options: ByteInnerSplitlinesOptions) -> Vec<&[u8]> {
        let keepends = options.get_value();

        let mut res = vec![];

        if self.elements.is_empty() {
            return vec![];
        }

        let mut prev_index = 0;
        let mut index = 0;
        let keep = if keepends { 1 } else { 0 };
        let slice = &self.elements;

        while index < slice.len() {
            match slice[index] {
                b'\n' => {
                    res.push(&slice[prev_index..index + keep]);
                    index += 1;
                    prev_index = index;
                }
                b'\r' => {
                    if index + 2 <= slice.len() && slice[index + 1] == b'\n' {
                        res.push(&slice[prev_index..index + keep + keep]);
                        index += 2;
                    } else {
                        res.push(&slice[prev_index..index + keep]);
                        index += 1;
                    }
                    prev_index = index;
                }
                _x => {
                    if index == slice.len() - 1 {
                        res.push(&slice[prev_index..=index]);
                        break;
                    }
                    index += 1
                }
            }
        }

        res
    }

    pub fn zfill(&self, width: isize) -> Vec<u8> {
        bytes_zfill(&self.elements, width.to_usize().unwrap_or(0))
    }

    pub fn replace(
        &self,
        old: PyByteInner,
        new: PyByteInner,
        count: OptionalArg<PyIntRef>,
    ) -> PyResult<Vec<u8>> {
        let count = match count.into_option() {
            Some(int) => int
                .as_bigint()
                .to_u32()
                .unwrap_or(self.elements.len() as u32),
            None => self.elements.len() as u32,
        };

        let mut res = vec![];
        let mut index = 0;
        let mut done = 0;

        let slice = &self.elements;
        loop {
            if done == count || index > slice.len() - old.len() {
                res.extend_from_slice(&slice[index..]);
                break;
            }
            if &slice[index..index + old.len()] == old.elements.as_slice() {
                res.extend_from_slice(&new.elements);
                index += old.len();
                done += 1;
            } else {
                res.push(slice[index]);
                index += 1
            }
        }

        Ok(res)
    }

    pub fn title(&self) -> Vec<u8> {
        let mut res = vec![];
        let mut spaced = true;

        for i in self.elements.iter() {
            match i {
                65..=90 | 97..=122 => {
                    if spaced {
                        res.push(i.to_ascii_uppercase());
                        spaced = false
                    } else {
                        res.push(i.to_ascii_lowercase());
                    }
                }
                _ => {
                    res.push(*i);
                    spaced = true
                }
            }
        }

        res
    }

    pub fn repeat(&self, n: isize) -> Vec<u8> {
        if self.elements.is_empty() || n <= 0 {
            // We can multiple an empty vector by any integer, even if it doesn't fit in an isize.
            Vec::new()
        } else {
            let n = usize::try_from(n).unwrap();

            let mut new_value = Vec::with_capacity(n * self.elements.len());
            for _ in 0..n {
                new_value.extend(&self.elements);
            }

            new_value
        }
    }

    pub fn irepeat(&mut self, n: isize) {
        if self.elements.is_empty() {
            // We can multiple an empty vector by any integer, even if it doesn't fit in an isize.
            return;
        }

        if n <= 0 {
            self.elements.clear();
        } else {
            let n = usize::try_from(n).unwrap();

            let old = self.elements.clone();

            self.elements.reserve((n - 1) * old.len());
            for _ in 1..n {
                self.elements.extend(&old);
            }
        }
    }
}

pub fn try_as_byte(obj: &PyObjectRef) -> Option<Vec<u8>> {
    match_class!(match obj.clone() {
        i @ PyBytes => Some(i.get_value().to_vec()),
        j @ PyByteArray => Some(j.borrow_value().elements.to_vec()),
        _ => None,
    })
}

pub trait ByteOr: ToPrimitive {
    fn byte_or(&self, vm: &VirtualMachine) -> PyResult<u8> {
        match self.to_u8() {
            Some(value) => Ok(value),
            None => Err(vm.new_value_error("byte must be in range(0, 256)".to_owned())),
        }
    }
}

impl ByteOr for BigInt {}

fn split_slice<'a>(slice: &'a [u8], sep: &[u8], maxsplit: isize) -> Vec<&'a [u8]> {
    let mut splitted: Vec<&[u8]> = vec![];
    let mut prev_index = 0;
    let mut index = 0;
    let mut count = 0;
    let mut in_string = false;

    // No sep given, will split for any \t \n \r and space  = [9, 10, 13, 32]
    if sep.is_empty() {
        // split wihtout sep always trim left spaces for any maxsplit
        // so we have to ignore left spaces.
        loop {
            if [9, 10, 13, 32].contains(&slice[index]) {
                index += 1
            } else {
                prev_index = index;
                break;
            }
        }

        // most simple case
        if maxsplit == 0 {
            splitted.push(&slice[index..slice.len()]);
            return splitted;
        }

        // main loop. in_string means previous char is ascii char(true) or space(false)
        // loop from left to right
        loop {
            if [9, 10, 13, 32].contains(&slice[index]) {
                if in_string {
                    splitted.push(&slice[prev_index..index]);
                    in_string = false;
                    count += 1;
                    if count == maxsplit {
                        // while index < slice.len()
                        splitted.push(&slice[index + 1..slice.len()]);
                        break;
                    }
                }
            } else if !in_string {
                prev_index = index;
                in_string = true;
            }

            index += 1;

            // handle last item in slice
            if index == slice.len() {
                if in_string {
                    if [9, 10, 13, 32].contains(&slice[index - 1]) {
                        splitted.push(&slice[prev_index..index - 1]);
                    } else {
                        splitted.push(&slice[prev_index..index]);
                    }
                }
                break;
            }
        }
    } else {
        // sep is given, we match exact slice
        while index != slice.len() {
            if index + sep.len() >= slice.len() {
                if &slice[index..slice.len()] == sep {
                    splitted.push(&slice[prev_index..index]);
                    splitted.push(&[]);
                    break;
                }
                splitted.push(&slice[prev_index..slice.len()]);
                break;
            }

            if &slice[index..index + sep.len()] == sep {
                splitted.push(&slice[prev_index..index]);
                index += sep.len();
                prev_index = index;
                count += 1;
                if count == maxsplit {
                    // maxsplit reached, append, the remaing
                    splitted.push(&slice[prev_index..slice.len()]);
                    break;
                }
                continue;
            }

            index += 1;
        }
    }
    splitted
}

fn split_slice_reverse<'a>(slice: &'a [u8], sep: &[u8], maxsplit: isize) -> Vec<&'a [u8]> {
    let mut splitted: Vec<&[u8]> = vec![];
    let mut prev_index = slice.len();
    let mut index = slice.len();
    let mut count = 0;

    // No sep given, will split for any \t \n \r and space  = [9, 10, 13, 32]
    if sep.is_empty() {
        //adjust index
        index -= 1;

        // rsplit without sep always trim right spaces for any maxsplit
        // so we have to ignore right spaces.
        loop {
            if [9, 10, 13, 32].contains(&slice[index]) {
                index -= 1
            } else {
                break;
            }
        }
        prev_index = index + 1;

        // most simple case
        if maxsplit == 0 {
            splitted.push(&slice[0..=index]);
            return splitted;
        }

        // main loop. in_string means previous char is ascii char(true) or space(false)
        // loop from right to left and reverse result the end
        let mut in_string = true;
        loop {
            if [9, 10, 13, 32].contains(&slice[index]) {
                if in_string {
                    splitted.push(&slice[index + 1..prev_index]);
                    count += 1;
                    if count == maxsplit {
                        // maxsplit reached, append, the remaing
                        splitted.push(&slice[0..index]);
                        break;
                    }
                    in_string = false;
                    index -= 1;
                    continue;
                }
            } else if !in_string {
                in_string = true;
                if index == 0 {
                    splitted.push(&slice[0..1]);
                    break;
                }
                prev_index = index + 1;
            }
            if index == 0 {
                break;
            }
            index -= 1;
        }
    } else {
        // sep is give, we match exact slice going backwards
        while index != 0 {
            if index <= sep.len() {
                if &slice[0..index] == sep {
                    splitted.push(&slice[index..prev_index]);
                    splitted.push(&[]);
                    break;
                }
                splitted.push(&slice[0..prev_index]);
                break;
            }
            if &slice[(index - sep.len())..index] == sep {
                splitted.push(&slice[index..prev_index]);
                index -= sep.len();
                prev_index = index;
                count += 1;
                if count == maxsplit {
                    // maxsplit reached, append, the remaing
                    splitted.push(&slice[0..prev_index]);
                    break;
                }
                continue;
            }

            index -= 1;
        }
    }
    splitted.reverse();
    splitted
}

pub enum PyBytesLike {
    Bytes(PyBytesRef),
    Bytearray(PyByteArrayRef),
}

impl TryFromObject for PyBytesLike {
    fn try_from_object(vm: &VirtualMachine, obj: PyObjectRef) -> PyResult<Self> {
        match_class!(match obj {
            b @ PyBytes => Ok(PyBytesLike::Bytes(b)),
            b @ PyByteArray => Ok(PyBytesLike::Bytearray(b)),
            obj => Err(vm.new_type_error(format!(
                "a bytes-like object is required, not {}",
                obj.class()
            ))),
        })
    }
}

impl PyBytesLike {
    pub fn to_cow(&self) -> std::borrow::Cow<[u8]> {
        match self {
            PyBytesLike::Bytes(b) => b.get_value().into(),
            PyBytesLike::Bytearray(b) => b.borrow_value().elements.clone().into(),
        }
    }

    #[inline]
    pub fn with_ref<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        match self {
            PyBytesLike::Bytes(b) => f(b.get_value()),
            PyBytesLike::Bytearray(b) => f(&b.borrow_value().elements),
        }
    }
}

pub fn bytes_zfill(bytes: &[u8], width: usize) -> Vec<u8> {
    if width <= bytes.len() {
        bytes.to_vec()
    } else {
        let (sign, s) = match bytes.first() {
            Some(_sign @ b'+') | Some(_sign @ b'-') => {
                (unsafe { bytes.get_unchecked(..1) }, &bytes[1..])
            }
            _ => (&b""[..], bytes),
        };
        let mut filled = Vec::new();
        filled.extend_from_slice(sign);
        filled.extend(std::iter::repeat(b'0').take(width - bytes.len()));
        filled.extend_from_slice(s);
        filled
    }
}
