//! Responsive terminal artwork derived from the repository's GrokForge ASCII mark.
//!
//! The source is a conventional dark-to-light ASCII raster: `@` is its background and a literal
//! space is its brightest pixel. We invert that density into foreground glyphs, crop the meaningful
//! mark, and average-pool it uniformly for smaller terminals. This keeps the logo recognizable
//! without painting the source's dense `@` background as a rectangle.

use std::sync::OnceLock;

const SOURCE: &str = include_str!("../../../assets/asci.txt");
const SOURCE_RAMP: &str = "@%#*+=-:. ";
const OUTPUT_RAMP: &[u8; 10] = b" .:-=+*#%@";
const SOURCE_WIDTH: usize = 117;
const SOURCE_HEIGHT: usize = 34;
const CROP_X: usize = 25;
const CROP_Y: usize = 1;
const CROP_WIDTH: usize = 66;
const CROP_HEIGHT: usize = 33;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtScale {
    Full,
    Half,
    Quarter,
}

impl ArtScale {
    const fn factor(self) -> usize {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
        }
    }

    pub(crate) const fn width(self) -> u16 {
        match self {
            Self::Full => 66,
            Self::Half => 33,
            Self::Quarter => 17,
        }
    }

    pub(crate) const fn height(self) -> u16 {
        match self {
            Self::Full => 33,
            Self::Half => 17,
            Self::Quarter => 9,
        }
    }
}

#[derive(Debug)]
pub(crate) struct BrandArt {
    lines: Vec<String>,
    width: u16,
    height: u16,
}

impl BrandArt {
    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }

    pub(crate) const fn width(&self) -> u16 {
        self.width
    }

    pub(crate) const fn height(&self) -> u16 {
        self.height
    }
}

static FULL: OnceLock<BrandArt> = OnceLock::new();
static HALF: OnceLock<BrandArt> = OnceLock::new();
static QUARTER: OnceLock<BrandArt> = OnceLock::new();

/// Select the largest uniformly scaled mark that fits the supplied rectangle.
pub(crate) fn responsive(max_width: u16, max_height: u16) -> Option<&'static BrandArt> {
    [ArtScale::Full, ArtScale::Half, ArtScale::Quarter]
        .into_iter()
        .find(|scale| scale.width() <= max_width && scale.height() <= max_height)
        .and_then(art)
}

fn art(scale: ArtScale) -> Option<&'static BrandArt> {
    let cell = match scale {
        ArtScale::Full => &FULL,
        ArtScale::Half => &HALF,
        ArtScale::Quarter => &QUARTER,
    };
    if let Some(art) = cell.get() {
        return Some(art);
    }
    let built = build(scale)?;
    let _ = cell.set(built);
    cell.get()
}

fn build(scale: ArtScale) -> Option<BrandArt> {
    let source = source_weights()?;
    let factor = scale.factor();
    let target_width = CROP_WIDTH.div_ceil(factor);
    let target_height = CROP_HEIGHT.div_ceil(factor);
    let mut lines = Vec::with_capacity(target_height);

    for target_y in 0..target_height {
        let mut line = String::with_capacity(target_width);
        for target_x in 0..target_width {
            let start_y = target_y.saturating_mul(factor);
            let start_x = target_x.saturating_mul(factor);
            let end_y = start_y.saturating_add(factor).min(CROP_HEIGHT);
            let end_x = start_x.saturating_add(factor).min(CROP_WIDTH);
            let mut sum = 0usize;
            let mut count = 0usize;
            for row in &source[start_y..end_y] {
                for weight in &row[start_x..end_x] {
                    sum = sum.saturating_add(*weight);
                    count = count.saturating_add(1);
                }
            }
            let rounded = sum.saturating_add(count / 2).checked_div(count)?;
            line.push(char::from(
                *OUTPUT_RAMP.get(rounded.min(OUTPUT_RAMP.len() - 1))?,
            ));
        }
        lines.push(line);
    }

    Some(BrandArt {
        lines,
        width: u16::try_from(target_width).ok()?,
        height: u16::try_from(target_height).ok()?,
    })
}

fn source_weights() -> Option<Vec<Vec<usize>>> {
    let rows = SOURCE
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect::<Vec<_>>();
    if rows.len() != SOURCE_HEIGHT || rows.iter().any(|row| row.len() != SOURCE_WIDTH) {
        return None;
    }

    rows.get(CROP_Y..CROP_Y.saturating_add(CROP_HEIGHT))?
        .iter()
        .map(|row| {
            row.as_bytes()
                .get(CROP_X..CROP_X.saturating_add(CROP_WIDTH))?
                .iter()
                .map(|byte| SOURCE_RAMP.as_bytes().iter().position(|item| item == byte))
                .collect::<Option<Vec<_>>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;

    #[test]
    fn supplied_ascii_asset_has_the_expected_raster_and_crop() {
        let rows = SOURCE.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), SOURCE_HEIGHT);
        assert!(rows.iter().all(|row| row.len() == SOURCE_WIDTH));
        assert_eq!(source_weights().unwrap().len(), CROP_HEIGHT);
        assert!(
            source_weights()
                .unwrap()
                .iter()
                .all(|row| row.len() == CROP_WIDTH)
        );
    }

    #[test]
    fn responsive_variants_are_exact_ascii_rectangles() {
        for (scale, width, height) in [
            (ArtScale::Full, 66, 33),
            (ArtScale::Half, 33, 17),
            (ArtScale::Quarter, 17, 9),
        ] {
            let art = art(scale).expect("valid bundled art");
            assert_eq!((art.width(), art.height()), (width, height));
            assert_eq!(art.lines().len(), usize::from(height));
            assert!(
                art.lines()
                    .iter()
                    .all(|line| line.is_ascii() && line.len() == usize::from(width))
            );
            assert!(art.lines().iter().any(|line| {
                line.bytes()
                    .any(|byte| matches!(byte, b'*' | b'#' | b'%' | b'@'))
            }));
        }
    }

    #[test]
    fn selection_never_distorts_or_clips_the_mark() {
        assert_eq!(responsive(66, 33).map(BrandArt::width), Some(66));
        assert_eq!(responsive(65, 32).map(BrandArt::width), Some(33));
        assert_eq!(responsive(32, 16).map(BrandArt::width), Some(17));
        assert!(responsive(16, 8).is_none());
    }
}
