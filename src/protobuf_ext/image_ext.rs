// color type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ColorSpace {
    Luma = 1,
    LumaA = 2,
    Rgb = 3,
    Rgba = 4,
    DigitalYuv = 5,
    Bgra = 6,
}

impl ColorSpace {
    pub fn num_channels(&self) -> usize {
        match self {
            ColorSpace::Luma => 1,
            ColorSpace::LumaA => 2,
            ColorSpace::Rgb => 3,
            ColorSpace::Rgba => 4,
            ColorSpace::DigitalYuv => 3,
            ColorSpace::Bgra => 4,
        }
    }
}

#[cfg(feature = "with-image")]
mod with_image {
    use super::*;
    use crate::{error::Error, protobuf::summary::Image};
    use image::{
        flat::SampleLayout, png::PngEncoder, ColorType, DynamicImage, FlatSamples, ImageBuffer,
        Pixel,
    };
    use std::{io::Cursor, ops::Deref};

    impl TryFrom<ColorType> for ColorSpace {
        type Error = Error;

        fn try_from(from: ColorType) -> Result<Self, Self::Error> {
            let color_space = match from {
                ColorType::L8 => ColorSpace::Luma,
                ColorType::La8 => ColorSpace::LumaA,
                ColorType::Rgb8 => ColorSpace::Rgb,
                ColorType::Rgba8 => ColorSpace::Rgba,
                ColorType::Bgra8 => ColorSpace::Bgra,
                ColorType::L16 => ColorSpace::Luma,
                ColorType::La16 => ColorSpace::LumaA,
                ColorType::Rgb16 => ColorSpace::Rgb,
                ColorType::Rgba16 => ColorSpace::Rgba,
                _ => {
                    return Err(Error::conversion("color space is not supported"));
                }
            };
            Ok(color_space)
        }
    }

    // DynamicImage to image

    impl TryFrom<&DynamicImage> for Image {
        type Error = Error;

        fn try_from(from: &DynamicImage) -> Result<Self, Self::Error> {
            use DynamicImage::*;
            let image = match from {
                ImageLuma8(buffer) => Self::try_from(buffer)?,
                ImageLumaA8(buffer) => Self::try_from(buffer)?,
                ImageRgb8(buffer) => Self::try_from(buffer)?,
                ImageRgba8(buffer) => Self::try_from(buffer)?,
                ImageBgra8(buffer) => Self::try_from(buffer)?,
                _ => {
                    return Err(Error::conversion("unsupported image type"));
                }
            };
            Ok(image)
        }
    }

    impl TryFrom<DynamicImage> for Image {
        type Error = Error;

        fn try_from(from: DynamicImage) -> Result<Self, Self::Error> {
            Self::try_from(&from)
        }
    }

    // FlatSamples to image

    impl<B> TryFrom<&FlatSamples<B>> for Image
    where
        B: AsRef<[u8]>,
    {
        type Error = Error;

        fn try_from(from: &FlatSamples<B>) -> Result<Self, Self::Error> {
            let FlatSamples {
                layout:
                    SampleLayout {
                        width,
                        height,
                        channels,
                        ..
                    },
                color_hint,
                ..
            } = *from;
            let color_type =
                color_hint.ok_or_else(|| Error::conversion("color_hint must not be None"))?;
            let colorspace = ColorSpace::try_from(color_type)?;
            let samples = (0..height)
                .flat_map(|y| (0..width).flat_map(move |x| (0..channels).map(move |c| (y, x, c))))
                .map(|(y, x, c)| *from.get_sample(c, x, y).unwrap())
                .collect::<Vec<_>>();

            let encoded_image_string = {
                let mut cursor = Cursor::new(vec![]);
                PngEncoder::new(&mut cursor)
                    .encode(&samples, width, height, color_type)
                    .map_err(|err| Error::conversion(format!("{:?}", err)))?;
                cursor.into_inner()
            };

            Ok(Image {
                height: height as i32,
                width: width as i32,
                colorspace: colorspace as i32,
                encoded_image_string,
            })
        }
    }

    impl<B> TryFrom<FlatSamples<B>> for Image
    where
        B: AsRef<[u8]>,
    {
        type Error = Error;

        fn try_from(from: FlatSamples<B>) -> Result<Self, Self::Error> {
            Self::try_from(&from)
        }
    }

    // ImageBuffer to image

    impl<P, C> TryFrom<&ImageBuffer<P, C>> for Image
    where
        P: 'static + Pixel<Subpixel = u8>,
        C: Deref<Target = [P::Subpixel]> + AsRef<[P::Subpixel]>,
    {
        type Error = Error;

        fn try_from(from: &ImageBuffer<P, C>) -> Result<Self, Self::Error> {
            Self::try_from(from.as_flat_samples())
        }
    }
}

#[cfg(feature = "with-tch")]
pub use with_tch::*;
#[cfg(feature = "with-tch")]
mod with_tch {
    use super::*;
    use crate::{error::Error, protobuf::summary::Image};
    use image::{png::PngEncoder, ColorType};
    use std::{io::Cursor, ops::Deref, slice};
    use tch::{Kind, Tensor};

    macro_rules! tensor_to_vec {
        ($tensor:ident, $ty:ident) => {
            unsafe {
                let numel = $tensor.numel();
                let mut data: Vec<$ty> = Vec::with_capacity(numel);
                let slice = slice::from_raw_parts_mut(data.as_mut_ptr(), numel);
                $tensor.copy_data(slice, numel);
                data.set_len(numel);
                data
            }
        };
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum TchChannelOrder {
        CHW,
        HWC,
    }

    #[derive(Debug, PartialEq)]
    pub enum TensorRef<'a> {
        Owned(Tensor),
        Ref(&'a Tensor),
    }

    impl<'a> Deref for TensorRef<'a> {
        type Target = Tensor;

        fn deref(&self) -> &Self::Target {
            match self {
                TensorRef::Owned(tensor) => tensor,
                TensorRef::Ref(tensor) => tensor,
            }
        }
    }

    impl From<Tensor> for TensorRef<'static> {
        fn from(from: Tensor) -> Self {
            Self::Owned(from)
        }
    }

    impl<'a> From<&'a Tensor> for TensorRef<'a> {
        fn from(from: &'a Tensor) -> Self {
            Self::Ref(from)
        }
    }

    #[derive(Debug, PartialEq)]
    pub struct TchTensorAsImage<'a> {
        color_space: ColorSpace,
        order: TchChannelOrder,
        tensor: TensorRef<'a>,
    }

    impl<'a> TchTensorAsImage<'a> {
        pub fn new<T>(
            color_space: ColorSpace,
            order: TchChannelOrder,
            tensor: T,
        ) -> Result<Self, Error>
        where
            T: Into<TensorRef<'a>>,
        {
            let tensor = tensor.into();
            let (s1, s2, s3) = tensor.size3().map_err(|_| -> Error {
                todo!();
            })?;
            let (sc, _sh, _sw) = match order {
                TchChannelOrder::CHW => (s1, s2, s3),
                TchChannelOrder::HWC => (s3, s1, s2),
            };

            if color_space.num_channels() != sc as usize {
                todo!();
            }

            Ok(Self {
                color_space,
                order,
                tensor,
            })
        }
    }

    // to Image
    impl<'a> TryFrom<TchTensorAsImage<'a>> for Image {
        type Error = Error;

        fn try_from(from: TchTensorAsImage) -> Result<Self, Self::Error> {
            use ColorSpace as S;
            use TchChannelOrder as O;

            // CHW to HWC
            let hwc_tensor = match from.order {
                O::HWC => from.tensor.shallow_clone(),
                O::CHW => from.tensor.f_permute(&[1, 2, 0])?,
            };
            let (nh, nw, _nc) = hwc_tensor.size3().unwrap();

            // normalize values to [0, 255]
            let normalized_tensor = normalized_tensor(&hwc_tensor)?;

            // encode image
            let encoded_image_string = {
                let samples = tensor_to_vec!(normalized_tensor, u8);
                let color_type = match from.color_space {
                    S::Luma => ColorType::L8,
                    S::Rgb => ColorType::Rgb8,
                    S::Rgba => ColorType::Rgba8,
                    _ => {
                        todo!();
                    }
                };
                let mut cursor = Cursor::new(vec![]);
                PngEncoder::new(&mut cursor)
                    .encode(&samples, nw as u32, nh as u32, color_type)
                    .map_err(|err| Error::conversion(format!("{:?}", err)))?;
                cursor.into_inner()
            };

            Ok(Image {
                height: nh as i32,
                width: nw as i32,
                colorspace: from.color_space as i32,
                encoded_image_string,
            })
        }
    }

    fn normalized_tensor(tensor: &Tensor) -> Result<Tensor, Error> {
        let kind = tensor.f_kind()?;

        let normalized_tensor = match kind {
            Kind::Uint8 => tensor.shallow_clone(),
            Kind::Float | Kind::Double => {
                // determine the scale and offset by min/max values
                let valid_values_mask = tensor.f_isfinite()?;
                let valid_values = tensor.f_masked_select(&valid_values_mask)?;
                let min_value = f64::from(valid_values.f_min()?);
                let max_value = f64::from(valid_values.f_max()?);

                let (scale, offset) = if min_value >= 0.0 {
                    let scale = 255.0 / max_value;
                    let offset = 0.0;
                    (scale, offset)
                } else {
                    let scale = 127.0 / max_value.max(-min_value);
                    let offset = 128.0;
                    (scale, offset)
                };

                tensor
                    .f_mul_scalar(scale)?
                    .f_add_scalar(offset)?
                    .f_to_kind(Kind::Uint8)?
            }
            _ => {
                return Err(Error::conversion(format!(
                    "the tensor with kind {:?} cannot converted to image",
                    kind
                )));
            }
        };

        Ok(normalized_tensor)
    }
}
