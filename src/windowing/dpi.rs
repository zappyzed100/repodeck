//! Small DPI helpers shared by `monitor` and, later, placement-restoration code
//! (PLAN.md §4.1: RepoDeck runs Per-Monitor DPI Awareness V2).

/// Scale factor implied by a DPI value, relative to the Windows baseline of 96 DPI.
pub fn scale_factor(dpi: u32) -> f64 {
    f64::from(dpi) / 96.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_factor_at_96_dpi_is_one() {
        assert!((scale_factor(96) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn scale_factor_at_150_percent_is_one_point_five() {
        assert!((scale_factor(144) - 1.5).abs() < 1e-9);
    }
}
