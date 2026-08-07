//! Small compatibility surface between the renderer and the cross-platform
//! rs-vips bindings. Keeping this translation here prevents binding-specific
//! option mechanics from leaking into the image pipeline.

pub use rs_vips::{VipsImage, error::Error as VipsError};

use rs_vips::Vips;

pub struct VipsApp;

impl VipsApp {
    pub fn new(name: &str, leak: bool) -> Result<Self, VipsError> {
        Vips::init(name)?;
        Vips::leak_set(leak);
        Ok(Self)
    }

    pub fn concurrency_set(&self, maximum: i32) {
        Vips::concurrency_set(maximum);
    }

    pub fn cache_set_max(&self, maximum: i32) {
        Vips::cache_set_max(maximum);
    }

    pub fn cache_set_max_mem(&self, maximum: usize) {
        Vips::cache_set_max_mem(maximum);
    }

    pub fn cache_set_max_files(&self, maximum: i32) {
        Vips::cache_set_max_files(maximum);
    }

    pub fn error_buffer(&self) -> Result<String, VipsError> {
        Vips::error_buffer()
    }

    pub fn error_clear(&self) {
        Vips::error_clear();
    }

    pub fn version_string(&self) -> Result<String, VipsError> {
        Vips::version_string()
    }
}

impl Drop for VipsApp {
    fn drop(&mut self) {
        Vips::thread_shutdown();
    }
}

pub mod ops {
    pub use rs_vips::enums::{
        Angle, BandFormat, Extend, Intent, Interpretation, OperationMath, OperationMath2,
        OperationRelational,
    };

    use rs_vips::{
        VipsImage,
        voption::{Setter, VOption},
    };

    type Result<T> = rs_vips::Result<T>;

    #[derive(Debug, Clone, Copy)]
    pub struct ExtractBandOptions {
        pub n: i32,
    }

    #[derive(Debug, Clone)]
    pub struct IccTransformOptions {
        pub intent: Intent,
        pub black_point_compensation: bool,
        pub embedded: bool,
        pub input_profile: Option<String>,
        pub depth: i32,
    }

    impl Default for IccTransformOptions {
        fn default() -> Self {
            Self {
                intent: Intent::Relative,
                black_point_compensation: false,
                embedded: false,
                input_profile: None,
                depth: 8,
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct FlattenOptions {
        pub background: Vec<f64>,
    }

    #[derive(Debug, Clone)]
    pub struct EmbedOptions {
        pub extend: Extend,
        pub background: Vec<f64>,
    }

    pub fn abs(image: &VipsImage) -> Result<VipsImage> {
        image.abs()
    }

    pub fn add(left: &VipsImage, right: &VipsImage) -> Result<VipsImage> {
        left.add(right)
    }

    pub fn addalpha(image: &VipsImage) -> Result<VipsImage> {
        image.addalpha()
    }

    pub fn autorot(image: &VipsImage) -> Result<VipsImage> {
        image.autorot()
    }

    pub fn bandjoin(images: &mut [VipsImage]) -> Result<VipsImage> {
        VipsImage::bandjoin(images)
    }

    pub fn cast(image: &VipsImage, format: BandFormat) -> Result<VipsImage> {
        image.cast(format)
    }

    pub fn colourspace(image: &VipsImage, space: Interpretation) -> Result<VipsImage> {
        image.colourspace(space)
    }

    pub fn copy(image: &VipsImage) -> Result<VipsImage> {
        image.copy()
    }

    pub fn premultiply(image: &VipsImage) -> Result<VipsImage> {
        image.premultiply()
    }

    pub fn rotate(image: &VipsImage, angle: f64) -> Result<VipsImage> {
        image.rotate(angle)
    }

    pub fn unpremultiply(image: &VipsImage) -> Result<VipsImage> {
        image.unpremultiply()
    }

    pub fn embed_with_opts(
        image: &VipsImage,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        options: &EmbedOptions,
    ) -> Result<VipsImage> {
        image.embed_with_opts(
            x,
            y,
            width,
            height,
            VOption::new()
                .set("extend", extend_nick(options.extend))
                .set("background", options.background.as_slice()),
        )
    }

    pub fn extract_area(
        image: &VipsImage,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
    ) -> Result<VipsImage> {
        image.extract_area(left, top, width, height)
    }

    pub fn extract_band(image: &VipsImage, band: i32) -> Result<VipsImage> {
        image.extract_band(band)
    }

    pub fn extract_band_with_opts(
        image: &VipsImage,
        band: i32,
        options: &ExtractBandOptions,
    ) -> Result<VipsImage> {
        image.extract_band_with_opts(band, VOption::new().set("n", options.n))
    }

    pub fn flatten_with_opts(image: &VipsImage, options: &FlattenOptions) -> Result<VipsImage> {
        image.flatten_with_opts(VOption::new().set("background", options.background.as_slice()))
    }

    pub fn gaussblur(image: &VipsImage, sigma: f64) -> Result<VipsImage> {
        image.gaussblur(sigma)
    }

    pub fn icc_transform_with_opts(
        image: &VipsImage,
        output_profile: &str,
        options: &IccTransformOptions,
    ) -> Result<VipsImage> {
        let mut option = VOption::new()
            .set("intent", intent_nick(options.intent))
            .set("black_point_compensation", options.black_point_compensation)
            .set("embedded", options.embedded)
            .set("depth", options.depth);
        if let Some(input_profile) = options.input_profile.as_deref() {
            option = option.set("input_profile", input_profile);
        }
        image.icc_transform_with_opts(output_profile, option)
    }

    pub fn ifthenelse(
        condition: &VipsImage,
        then_image: &VipsImage,
        else_image: &VipsImage,
    ) -> Result<VipsImage> {
        condition.ifthenelse(then_image, else_image)
    }

    pub fn linear(image: &VipsImage, a: &mut [f64], b: &mut [f64]) -> Result<VipsImage> {
        image.linear(a, b)
    }

    pub fn maplut(image: &VipsImage, lut: &VipsImage) -> Result<VipsImage> {
        image.maplut(lut)
    }

    pub fn math2_const(
        image: &VipsImage,
        operation: OperationMath2,
        constants: &mut [f64],
    ) -> Result<VipsImage> {
        image.math2_const(operation, constants)
    }

    pub fn math(image: &VipsImage, operation: OperationMath) -> Result<VipsImage> {
        image.math(operation)
    }

    pub fn divide(left: &VipsImage, right: &VipsImage) -> Result<VipsImage> {
        left.divide(right)
    }

    pub fn multiply(left: &VipsImage, right: &VipsImage) -> Result<VipsImage> {
        left.multiply(right)
    }

    #[cfg(test)]
    pub fn pngsave(image: &VipsImage, filename: &str) -> Result<()> {
        image.pngsave(filename)
    }

    pub fn recomb(image: &VipsImage, matrix: &VipsImage) -> Result<VipsImage> {
        image.recomb(matrix)
    }

    pub fn relational_const(
        image: &VipsImage,
        operation: OperationRelational,
        constants: &mut [f64],
    ) -> Result<VipsImage> {
        image.relational_const(operation, constants)
    }

    pub fn resize(image: &VipsImage, scale: f64) -> Result<VipsImage> {
        image.resize(scale)
    }

    pub fn rot(image: &VipsImage, angle: Angle) -> Result<VipsImage> {
        image.rot(angle)
    }

    pub fn s_rgb2sc_rgb(image: &VipsImage) -> Result<VipsImage> {
        image.sRGB2scRGB()
    }

    pub fn sc_rgb2s_rgb(image: &VipsImage) -> Result<VipsImage> {
        image.scRGB2sRGB()
    }

    pub fn subtract(left: &VipsImage, right: &VipsImage) -> Result<VipsImage> {
        left.subtract(right)
    }

    const fn extend_nick(extend: Extend) -> &'static str {
        match extend {
            Extend::Black => "black",
            Extend::Copy => "copy",
            Extend::Repeat => "repeat",
            Extend::Mirror => "mirror",
            Extend::White => "white",
            Extend::Background => "background",
        }
    }

    const fn intent_nick(intent: Intent) -> &'static str {
        match intent {
            Intent::Perceptual => "perceptual",
            Intent::Relative => "relative",
            Intent::Saturation => "saturation",
            Intent::Absolute => "absolute",
            Intent::Auto => "auto",
        }
    }
}
