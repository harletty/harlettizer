// SPDX-License-Identifier: GPL-3.0-or-later
//
// Carries material ported from the EBU ADM Renderer —
// Copyright (c) 2018-2019 EBU ADM Renderer Authors — under its BSD 3-Clause
// licence, whose text is reproduced in LICENSES/EBU-ADM-Renderer.txt.
// What was taken and what was changed is recorded in docs/provenance.md.

//! Polar and Cartesian ADM positions, and the mapping between them.
//!
//! ITU-R BS.2127-0 §10. Not a spherical-to-rectangular conversion — that would
//! be four lines — but an *allocentric* one: the two coordinate systems
//! describe rooms of different shapes, a sphere and a cube, and the mapping
//! has to agree with what a renderer would do with each.
//!
//! The construction is a five-corner map of the horizontal plane. Front is
//! azimuth 0 at the cube's front edge midpoint, the front corners sit at ∓30°
//! and the rear ones at ±110°, and an azimuth between two of them is
//! interpolated along the cube wall by a tangent law that matches the panning
//! a renderer applies. Elevation is warped separately: 30° of polar elevation
//! reaches 45° on the cube, and everything above that is squeezed into what is
//! left up to the ceiling.
//!
//! # Conventions, which are the usual place to go wrong
//!
//! Azimuth is degrees **anticlockwise** from front, so +90 is the left side.
//! Cartesian X is to the right, Y to the front, Z up. Together those mean
//! `x = sin(-azimuth)·cos(elevation)·distance`, with the sign that catches
//! everybody.
//!
//! # Provenance
//!
//! Ported from the EBU ADM Renderer's implementation of the same section
//! (BSD-3-Clause), and checked numerically against it. Recorded in
//! `docs/provenance.md`.

/// The corners of the horizontal map: an azimuth, and the cube position it
/// corresponds to.
///
/// Front, then clockwise: the front right corner at −30°, the rear right at
/// −110°, the rear left at +110°, the front left at +30°.
#[rustfmt::skip]
const MAP: [(f64, [f64; 2]); 5] = [
    (   0.0, [ 0.0,  1.0]),
    ( -30.0, [ 1.0,  1.0]),
    (-110.0, [ 1.0, -1.0]),
    ( 110.0, [-1.0, -1.0]),
    (  30.0, [-1.0,  1.0]),
];

/// Where the polar elevation scale stops being linear.
const EL_TOP: f64 = 30.0;
/// What that elevation becomes on the cube.
const EL_TOP_TILDE: f64 = 45.0;

/// The azimuth of a Cartesian position, in degrees anticlockwise from front.
pub fn azimuth_of(x: f64, y: f64) -> f64 {
    (-x).atan2(y).to_degrees()
}

/// Shift `y` by whole turns until it is the smallest angle not below `x`.
pub fn relative_angle(x: f64, mut y: f64) -> f64 {
    while y - 360.0 >= x {
        y -= 360.0;
    }
    while y < x {
        y += 360.0;
    }
    y
}

/// Whether `angle` lies in the range that runs clockwise from `start` to `end`.
///
/// The two normalisations are not interchangeable with a simple modulo: a
/// range of (−180, 180) has to mean every angle while (−180, −180) means one
/// angle, and only shifting `end` relative to `start` preserves that.
pub fn inside_angle_range(mut angle: f64, start: f64, mut end: f64) -> bool {
    while end - 360.0 > start {
        end -= 360.0;
    }
    while end < start {
        end += 360.0;
    }
    while angle - 360.0 >= start {
        angle -= 360.0;
    }
    while angle < start {
        angle += 360.0;
    }
    angle <= end
}

/// Map an azimuth within a sector onto the linear position along its wall.
fn map_az_to_linear(left_az: f64, right_az: f64, azimuth: f64) -> f64 {
    let mid_az = (left_az + right_az) / 2.0;
    let az_range = right_az - mid_az;
    let rel_az = azimuth - mid_az;

    let gain_r = 0.5 + 0.5 * rel_az.to_radians().tan() / az_range.to_radians().tan();
    gain_r.atan2(1.0 - gain_r) * (2.0 / std::f64::consts::PI)
}

/// The inverse: a linear position along a wall back to an azimuth.
fn map_linear_to_az(left_az: f64, right_az: f64, x: f64) -> f64 {
    let mid_az = (left_az + right_az) / 2.0;
    let az_range = right_az - mid_az;

    let (gain_l, gain_r) = {
        let angle = x * std::f64::consts::FRAC_PI_2;
        (angle.cos(), angle.sin())
    };
    let gain_r = gain_r / (gain_l + gain_r);

    let rel_az = (2.0 * (gain_r - 0.5) * az_range.to_radians().tan())
        .atan()
        .to_degrees();
    mid_az + rel_az
}

/// The two map corners an azimuth falls between.
fn find_sector(az: f64) -> ((f64, [f64; 2]), (f64, [f64; 2])) {
    for (index, corner) in MAP.iter().enumerate() {
        let next = MAP[(index + 1) % MAP.len()];
        if inside_angle_range(az, next.0, corner.0) {
            return (*corner, next);
        }
    }
    // Unreachable: the map covers the circle. Falling back on front rather
    // than panicking keeps a bad input from taking a render down.
    (MAP[0], MAP[1])
}

/// The two map corners a Cartesian direction falls between.
fn find_cart_sector(az: f64) -> ((f64, [f64; 2]), (f64, [f64; 2])) {
    for (index, corner) in MAP.iter().enumerate() {
        let next = MAP[(index + 1) % MAP.len()];
        let start = azimuth_of(next.1[0], next.1[1]);
        let end = azimuth_of(corner.1[0], corner.1[1]);
        if inside_angle_range(az, start, end) {
            return (*corner, next);
        }
    }
    (MAP[0], MAP[1])
}

/// Polar to Cartesian, BS.2127-0 §10.
pub fn polar_to_cartesian(azimuth: f64, elevation: f64, distance: f64) -> [f64; 3] {
    let (z, r_xy) = if elevation.abs() > EL_TOP {
        // Above the tilt point, the object rides the ceiling and its
        // horizontal radius shrinks rather than its height growing.
        let el_tilde =
            EL_TOP_TILDE + (90.0 - EL_TOP_TILDE) * (elevation.abs() - EL_TOP) / (90.0 - EL_TOP);
        (
            distance * elevation.signum(),
            distance * (90.0 - el_tilde).to_radians().tan(),
        )
    } else {
        let el_tilde = EL_TOP_TILDE * elevation / EL_TOP;
        (el_tilde.to_radians().tan() * distance, distance)
    };

    let ((left_az, left_pos), (right_az, right_pos)) = find_sector(azimuth);
    let rel_az = relative_angle(right_az, azimuth);
    let rel_left_az = relative_angle(right_az, left_az);
    let p = map_az_to_linear(rel_left_az, right_az, rel_az);

    [
        r_xy * (left_pos[0] + (right_pos[0] - left_pos[0]) * p),
        r_xy * (left_pos[1] + (right_pos[1] - left_pos[1]) * p),
        z,
    ]
}

/// Cartesian to polar, BS.2127-0 §10.
pub fn cartesian_to_polar(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    const EPSILON: f64 = 1e-10;

    if x.abs() < EPSILON && y.abs() < EPSILON {
        // Directly overhead or underfoot, where azimuth means nothing.
        if z.abs() < EPSILON {
            return (0.0, 0.0, 0.0);
        }
        return (0.0, z.signum() * 90.0, z.abs());
    }

    let ((left_az, left_pos), (right_az, right_pos)) = find_cart_sector(azimuth_of(x, y));

    // Which mix of the two corners lands on this point.
    let determinant = left_pos[0] * right_pos[1] - left_pos[1] * right_pos[0];
    let gain_left = (x * right_pos[1] - y * right_pos[0]) / determinant;
    let gain_right = (y * left_pos[0] - x * left_pos[1]) / determinant;
    let r_xy = gain_left + gain_right;

    let rel_left_az = relative_angle(right_az, left_az);
    let az = map_linear_to_az(rel_left_az, right_az, gain_right / r_xy);
    let az = relative_angle(-180.0, az);

    let el_tilde = (z / r_xy).atan().to_degrees();

    let (elevation, distance) = if el_tilde.abs() > EL_TOP_TILDE {
        let abs_el =
            EL_TOP + (90.0 - EL_TOP) * (el_tilde.abs() - EL_TOP_TILDE) / (90.0 - EL_TOP_TILDE);
        (el_tilde.signum() * abs_el, z.abs())
    } else {
        (EL_TOP * el_tilde / EL_TOP_TILDE, r_xy)
    };

    (az, elevation, distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() < tolerance
    }

    /// Positive azimuth is anticlockwise, so it is to the left, so X goes
    /// negative. This one sign is the difference between a room and its mirror.
    #[test]
    fn positive_azimuth_is_the_left_side() {
        let [x, _, _] = polar_to_cartesian(90.0, 0.0, 1.0);
        assert!(x < 0.0, "azimuth +90 gave x = {x}");

        let [x, _, _] = polar_to_cartesian(-90.0, 0.0, 1.0);
        assert!(x > 0.0, "azimuth -90 gave x = {x}");

        assert!(close(azimuth_of(-1.0, 0.0), 90.0, 1e-9));
        assert!(close(azimuth_of(0.0, 1.0), 0.0, 1e-9));
    }

    /// The five corners of the map have to land exactly on the cube corners
    /// they are defined as, or every position between them is off.
    #[test]
    fn the_map_corners_land_on_the_cube() {
        for (azimuth, expected) in MAP {
            let [x, y, z] = polar_to_cartesian(azimuth, 0.0, 1.0);
            assert!(close(x, expected[0], 1e-9), "az {azimuth}: x {x}");
            assert!(close(y, expected[1], 1e-9), "az {azimuth}: y {y}");
            assert!(close(z, 0.0, 1e-9), "az {azimuth}: z {z}");
        }
    }

    /// Straight up is the cube's ceiling, straight ahead its front wall.
    #[test]
    fn the_cardinal_directions_are_where_they_should_be() {
        let [x, y, z] = polar_to_cartesian(0.0, 90.0, 1.0);
        assert!(close(x, 0.0, 1e-9) && close(y, 0.0, 1e-9) && close(z, 1.0, 1e-9));

        let [x, y, z] = polar_to_cartesian(0.0, 0.0, 1.0);
        assert!(close(x, 0.0, 1e-9) && close(y, 1.0, 1e-9) && close(z, 0.0, 1e-9));
    }

    /// The elevation warp is the part that is not a spherical conversion: 30°
    /// of polar elevation reaches 45° on the cube.
    #[test]
    fn the_elevation_warp_is_applied() {
        let [_, y, z] = polar_to_cartesian(0.0, EL_TOP, 1.0);
        let cube_elevation = z.atan2(y).to_degrees();
        assert!(
            close(cube_elevation, EL_TOP_TILDE, 1e-6),
            "{EL_TOP}° became {cube_elevation}°"
        );

        // A plain spherical conversion would have left it at 30.
        assert!(cube_elevation > EL_TOP + 10.0);
    }

    /// Both directions have to agree, or converting a file twice moves
    /// everything in it.
    #[test]
    fn the_conversion_is_its_own_inverse() {
        let mut checked = 0;
        for azimuth in (-180..180).step_by(7) {
            for elevation in (-90..=90).step_by(6) {
                for distance in [0.25, 0.5, 1.0] {
                    let (azimuth, elevation) = (f64::from(azimuth), f64::from(elevation));
                    let [x, y, z] = polar_to_cartesian(azimuth, elevation, distance);
                    let (az, el, d) = cartesian_to_polar(x, y, z);

                    assert!(close(d, distance, 1e-9), "distance {d} vs {distance}");
                    assert!(close(el, elevation, 1e-9), "elevation {el} vs {elevation}");
                    // Straight up and down have no azimuth to preserve.
                    if elevation.abs() < 90.0 {
                        let difference = relative_angle(-180.0, az - azimuth);
                        assert!(
                            close(difference, 0.0, 1e-9),
                            "azimuth {az} vs {azimuth} at el {elevation}"
                        );
                    }
                    checked += 1;
                }
            }
        }
        assert!(checked > 1000, "only {checked} positions checked");
    }

    #[test]
    fn the_origin_and_the_poles_are_handled_rather_than_dividing_by_zero() {
        assert_eq!(cartesian_to_polar(0.0, 0.0, 0.0), (0.0, 0.0, 0.0));

        let (az, el, d) = cartesian_to_polar(0.0, 0.0, 1.0);
        assert!(close(az, 0.0, 1e-9) && close(el, 90.0, 1e-9) && close(d, 1.0, 1e-9));

        let (_, el, d) = cartesian_to_polar(0.0, 0.0, -0.5);
        assert!(close(el, -90.0, 1e-9) && close(d, 0.5, 1e-9));
    }

    #[test]
    fn an_angle_range_that_spans_the_circle_contains_everything() {
        assert!(inside_angle_range(0.0, -180.0, 180.0));
        assert!(inside_angle_range(179.0, -180.0, 180.0));
        // …while a degenerate range contains only its own angle.
        assert!(inside_angle_range(-180.0, -180.0, -180.0));
        assert!(!inside_angle_range(0.0, -180.0, -180.0));
    }
}
