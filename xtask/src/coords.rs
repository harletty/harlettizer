//! Print the polar/Cartesian conversion over a grid, so another
//! implementation's answers can be held next to ours.
//!
//! BS.2127-0 §10 is not a conversion anybody should trust themselves to have
//! read correctly. The round-trip test proves the two directions agree with
//! each other, which two wrong functions can also do. This prints numbers a
//! reference implementation can be asked the same questions.

use hz_core::Result;
use hz_core::coords;

pub struct Options {
    /// Degrees between sampled azimuths.
    pub step: usize,
}

pub fn run(options: Options) -> Result<()> {
    let step = options.step.max(1);
    println!("azimuth,elevation,distance,x,y,z,az_back,el_back,dist_back");

    let mut azimuth = -180;
    while azimuth < 180 {
        let mut elevation = -90;
        while elevation <= 90 {
            for distance in [0.5, 1.0] {
                let (a, e) = (f64::from(azimuth), f64::from(elevation));
                let [x, y, z] = coords::polar_to_cartesian(a, e, distance);
                let (back_a, back_e, back_d) = coords::cartesian_to_polar(x, y, z);
                println!(
                    "{a},{e},{distance},{x:.12},{y:.12},{z:.12},{back_a:.12},{back_e:.12},{back_d:.12}"
                );
            }
            elevation += step as i32;
        }
        azimuth += step as i32;
    }
    Ok(())
}
