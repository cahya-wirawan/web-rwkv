use bytemuck::Pod;
use derive_getters::Getters;
use half::prelude::*;
use std::{borrow::Cow, marker::PhantomData, num::NonZeroU64, sync::Arc};
use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    BindingResource, Buffer, BufferAddress, BufferBinding, BufferDescriptor, BufferUsages,
    CommandEncoder, MaintainBase, MapMode,
};

use crate::Context;

#[derive(Debug, Clone)]
pub struct TensorBuffer {
    pub buffer: Arc<Buffer>,
    pub offset: BufferAddress,
}

pub trait Device: sealed::Sealed {
    type Data: Clone;
}

pub struct Cpu<'a, T>(&'a PhantomData<T>);
pub struct Gpu;

impl<'a, T: Scalar> Device for Cpu<'a, T> {
    type Data = Cow<'a, [T]>;
}

impl Device for Gpu {
    type Data = TensorBuffer;
}

pub trait Scalar: Sized + Clone + Copy + Pod + sealed::Sealed {
    fn byte_size() -> usize {
        std::mem::size_of::<Self>()
    }
}

impl Scalar for f32 {}
impl Scalar for f16 {}
impl Scalar for u8 {}

/// The shape of a [`Tensor`].
/// Note that the fastest-moving axis occupies the lowest shape index, which is opposite to that in `torch`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TensorShape([usize; 4]);

impl TensorShape {
    pub fn len(&self) -> usize {
        self.0.into_iter().product()
    }
}

impl std::fmt::Display for TensorShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {}, {}, {})", self[0], self[1], self[2], self[3])
    }
}

impl std::ops::Index<usize> for TensorShape {
    type Output = usize;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl std::ops::IndexMut<usize> for TensorShape {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.0[index]
    }
}

#[derive(Debug, Clone, Copy)]
pub enum TensorError {
    Size(usize, usize),
    Shape(TensorShape, TensorShape),
    Overflow {
        buffer_size: BufferAddress,
        offset: BufferAddress,
        size: BufferAddress,
    },
    DeviceError,
}

impl std::fmt::Display for TensorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TensorError::Size(a, b) => write!(f, "Data size not match: {} vs. {}", a, b),
            TensorError::Shape(a, b) => write!(f, "Tensor shape not match: {} vs. {}", a, b),
            TensorError::Overflow {
                buffer_size,
                offset,
                size,
            } => write!(
                f,
                "Buffer overflow with buffer size: {}, slice offset: {} and size: {}",
                buffer_size, offset, size
            ),
            TensorError::DeviceError => write!(f, "Tensor not on the same device"),
        }
    }
}

impl std::error::Error for TensorError {}

#[derive(Debug, Clone, Getters)]
pub struct Tensor<'a, D: Device, T> {
    context: Context,
    shape: TensorShape,
    name: Option<&'a str>,
    data: D::Data,
    #[getter(skip)]
    phantom: std::marker::PhantomData<(D, T)>,
}

impl<D: Device, T: Scalar> std::ops::Deref for Tensor<'_, D, T> {
    type Target = D::Data;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<D: Device, T: Scalar> Tensor<'_, D, T> {
    pub fn byte_size(&self) -> usize {
        self.shape.len() * T::byte_size()
    }

    pub fn byte_offset(offset: usize) -> usize {
        offset * T::byte_size()
    }

    pub fn shape_index(&self, indices: TensorShape) -> usize {
        let mut index = indices[3];
        index = index * self.shape[2] + indices[2];
        index = index * self.shape[1] + indices[1];
        index = index * self.shape[0] + indices[0];
        index
    }
}

impl<'a, T: Scalar> TensorCpu<'a, T> {
    pub fn new(
        context: Context,
        shape: TensorShape,
        name: Option<&'a str>,
        data: Vec<T>,
    ) -> Result<Self, TensorError> {
        if shape.len() != data.len() {
            return Err(TensorError::Size(shape.len(), data.len()));
        }
        Ok(Self {
            context,
            shape,
            name,
            data: Cow::from(data),
            phantom: Default::default(),
        })
    }
}

impl<'a, T: Scalar> std::ops::Index<usize> for TensorCpu<'a, T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        &self.data[index]
    }
}

impl<'a, T: Scalar> std::ops::Index<std::ops::Range<usize>> for TensorCpu<'a, T> {
    type Output = [T];

    fn index(&self, index: std::ops::Range<usize>) -> &Self::Output {
        &self.data[index]
    }
}

impl<'a, T: Scalar> TensorGpu<'a, T> {
    /// Create a GPU tensor from a [`BufferView`].
    /// Fails if the buffer overflows.
    pub fn new(
        context: Context,
        shape: TensorShape,
        name: Option<&'a str>,
        data: TensorBuffer,
    ) -> Result<Self, TensorError> {
        let size = shape.len() as u64 * T::byte_size() as u64;
        if data.offset + size >= data.buffer.size() {
            return Err(TensorError::Overflow {
                buffer_size: data.buffer.size(),
                offset: data.offset,
                size,
            });
        }
        Ok(Self {
            context,
            shape,
            name,
            data,
            phantom: Default::default(),
        })
    }

    /// Initialize a GPU tensor with a given shape.
    pub fn init(
        context: Context,
        shape: TensorShape,
        name: Option<&'a str>,
        usage: BufferUsages,
    ) -> Self {
        let label = name;
        let size = shape.len() as u64 * T::byte_size() as u64;
        let buffer = context
            .device
            .create_buffer(&BufferDescriptor {
                label,
                size,
                usage,
                mapped_at_creation: false,
            })
            .into();
        Self {
            context,
            shape,
            name,
            data: TensorBuffer { buffer, offset: 0 },
            phantom: Default::default(),
        }
    }

    pub fn binding(&self) -> BindingResource {
        BindingResource::Buffer(BufferBinding {
            buffer: &self.buffer,
            offset: self.offset,
            size: NonZeroU64::new(self.byte_size() as BufferAddress),
        })
    }
}

impl<'a, T: Scalar> From<TensorCpu<'a, T>> for TensorGpu<'a, T> {
    fn from(value: TensorCpu<'a, T>) -> Self {
        let Tensor {
            context,
            shape,
            name,
            data,
            ..
        } = value;
        let label = name;
        let contents = bytemuck::cast_slice(&data);
        let buffer = context
            .device
            .create_buffer_init(&BufferInitDescriptor {
                label,
                contents,
                usage: BufferUsages::STORAGE | BufferUsages::COPY_DST | BufferUsages::COPY_SRC,
            })
            .into();
        Self {
            context,
            shape,
            name,
            data: TensorBuffer { buffer, offset: 0 },
            phantom: Default::default(),
        }
    }
}

impl<'a, T: Scalar> From<TensorGpu<'a, T>> for TensorCpu<'a, T> {
    fn from(value: TensorGpu<'a, T>) -> Self {
        let size = value.byte_size() as u64;
        let Tensor {
            context,
            shape,
            name,
            data: TensorBuffer { buffer, offset },
            ..
        } = value;

        let slice = buffer.slice(offset..offset + size);
        slice.map_async(MapMode::Read, |_| ());

        context.device.poll(MaintainBase::Wait);

        let map = slice.get_mapped_range();
        let data = Cow::from(bytemuck::cast_slice(&map).to_owned());
        buffer.unmap();

        Self {
            context,
            shape,
            name,
            data,
            phantom: Default::default(),
        }
    }
}

pub trait CopyTensor<Source, Destination> {
    fn copy_tensor(&mut self, src: &Source, dst: &Destination) -> Result<(), TensorError>;
}

impl<T: Scalar> CopyTensor<TensorGpu<'_, T>, TensorGpu<'_, T>> for CommandEncoder {
    fn copy_tensor(
        &mut self,
        src: &TensorGpu<'_, T>,
        dst: &TensorGpu<'_, T>,
    ) -> Result<(), TensorError> {
        if src.shape != dst.shape {
            return Err(TensorError::Shape(src.shape, dst.shape));
        }
        let size = src.byte_size() as BufferAddress;
        self.copy_buffer_to_buffer(&src.buffer, src.offset, &dst.buffer, dst.offset, size);
        Ok(())
    }
}

pub type TensorCpu<'a, T> = Tensor<'a, Cpu<'a, T>, T>;
pub type TensorGpu<'a, T> = Tensor<'a, Gpu, T>;

mod sealed {
    use super::{Cpu, Gpu};
    use half::prelude::f16;

    pub trait Sealed {}

    impl<T> Sealed for Cpu<'_, T> {}
    impl Sealed for Gpu {}

    impl Sealed for f32 {}
    impl Sealed for f16 {}
    impl Sealed for u8 {}
}
