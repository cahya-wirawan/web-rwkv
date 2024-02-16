use bytemuck::Pod;
use half::f16;
use safetensors::Dtype;

pub trait Zero: Sized + core::ops::Add<Self, Output = Self> {
    fn zero() -> Self;
}

impl Zero for f32 {
    fn zero() -> Self {
        0.0
    }
}

impl Zero for f16 {
    fn zero() -> Self {
        Self::ZERO
    }
}

impl Zero for u8 {
    fn zero() -> Self {
        0
    }
}

impl Zero for u16 {
    fn zero() -> Self {
        0
    }
}

impl Zero for u32 {
    fn zero() -> Self {
        0
    }
}

pub trait One: Sized + core::ops::Mul<Self, Output = Self> {
    fn one() -> Self;
}

impl One for f32 {
    fn one() -> Self {
        1.0
    }
}

impl One for f16 {
    fn one() -> Self {
        Self::ONE
    }
}

impl One for u8 {
    fn one() -> Self {
        1
    }
}

impl One for u16 {
    fn one() -> Self {
        1
    }
}

impl One for u32 {
    fn one() -> Self {
        1
    }
}

pub trait Scalar: Sized + Clone + Copy + Pod + Zero + One + sealed::Sealed {
    /// Size of the type in bytes.
    fn size() -> usize {
        std::mem::size_of::<Self>()
    }

    const DATA_TYPE: Dtype;
}

impl Scalar for f32 {
    const DATA_TYPE: Dtype = Dtype::F32;
}
impl Scalar for f16 {
    const DATA_TYPE: Dtype = Dtype::F16;
}
impl Scalar for u8 {
    const DATA_TYPE: Dtype = Dtype::U8;
}
impl Scalar for u16 {
    const DATA_TYPE: Dtype = Dtype::U16;
}
impl Scalar for u32 {
    const DATA_TYPE: Dtype = Dtype::U32;
}

pub trait Float: Scalar + Hom<f16> + Hom<f32> {
    const DEF: &'static str;
}

impl Float for f32 {
    const DEF: &'static str = "FP32";
}

impl Float for f16 {
    const DEF: &'static str = "FP16";
}

pub trait Hom<T> {
    fn hom(self) -> T;
    fn co_hom(value: T) -> Self;
}

impl Hom<f32> for f32 {
    fn hom(self) -> f32 {
        self
    }

    fn co_hom(value: f32) -> Self {
        value
    }
}

impl Hom<f16> for f32 {
    fn hom(self) -> f16 {
        f16::from_f32(self)
    }

    fn co_hom(value: f16) -> Self {
        value.to_f32()
    }
}

impl Hom<f32> for f16 {
    fn hom(self) -> f32 {
        self.to_f32()
    }

    fn co_hom(value: f32) -> Self {
        Self::from_f32(value)
    }
}

impl Hom<f16> for f16 {
    fn hom(self) -> f16 {
        self
    }

    fn co_hom(value: f16) -> Self {
        value
    }
}

mod sealed {
    use half::f16;

    pub trait Sealed {}

    impl Sealed for f32 {}
    impl Sealed for f16 {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
}
