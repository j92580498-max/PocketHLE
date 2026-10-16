//! OpenGL ES 1.x lighting state; software and native shaders evaluate it per vertex.
use crate::{
    consts::*,
    matrix::{self, Matrix4, IDENTITY},
};

pub const AMBIENT: u32 = 0x1200;
pub const DIFFUSE: u32 = 0x1201;
pub const SPECULAR: u32 = 0x1202;
pub const POSITION: u32 = 0x1203;
pub const SPOT_DIRECTION: u32 = 0x1204;
pub const SPOT_EXPONENT: u32 = 0x1205;
pub const SPOT_CUTOFF: u32 = 0x1206;
pub const CONSTANT_ATTENUATION: u32 = 0x1207;
pub const LINEAR_ATTENUATION: u32 = 0x1208;
pub const QUADRATIC_ATTENUATION: u32 = 0x1209;
pub const EMISSION: u32 = 0x1600;
pub const SHININESS: u32 = 0x1601;
pub const AMBIENT_AND_DIFFUSE: u32 = 0x1602;
pub const MODEL_AMBIENT: u32 = 0x0b53;
pub const MODEL_TWO_SIDE: u32 = 0x0b52;
pub const RESCALE_NORMAL: u32 = 0x803a;

#[derive(Clone, Copy, Debug)]
pub struct Light {
    pub enabled: bool,
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub position: [f32; 4],
    pub spot_direction: [f32; 3],
    pub spot_exponent: f32,
    pub spot_cutoff: f32,
    pub attenuation: [f32; 3],
}
impl Default for Light {
    fn default() -> Self {
        Self {
            enabled: false,
            ambient: [0., 0., 0., 1.],
            diffuse: [0.; 4],
            specular: [0.; 4],
            position: [0., 0., 1., 0.],
            spot_direction: [0., 0., -1.],
            spot_exponent: 0.,
            spot_cutoff: 180.,
            attenuation: [1., 0., 0.],
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct LightingState {
    pub enabled: bool,
    pub normalize: bool,
    pub rescale: bool,
    pub color_material: bool,
    pub two_sided: bool,
    pub model_ambient: [f32; 4],
    pub ambient: [f32; 4],
    pub diffuse: [f32; 4],
    pub specular: [f32; 4],
    pub emission: [f32; 4],
    pub shininess: f32,
    pub lights: [Light; 8],
}
impl Default for LightingState {
    fn default() -> Self {
        let mut lights = [Light::default(); 8];
        lights[0].diffuse = [1.; 4];
        lights[0].specular = [1.; 4];
        Self {
            enabled: false,
            normalize: false,
            rescale: false,
            color_material: false,
            two_sided: false,
            model_ambient: [0.2, 0.2, 0.2, 1.],
            ambient: [0.2, 0.2, 0.2, 1.],
            diffuse: [0.8, 0.8, 0.8, 1.],
            specular: [0., 0., 0., 1.],
            emission: [0., 0., 0., 1.],
            shininess: 0.,
            lights,
        }
    }
}
#[derive(Clone, Copy)]
pub enum Target {
    Light(u32),
    Material(u32),
    Model,
}

pub fn parameter_count(target: Target, pname: u32, scalar: bool) -> Result<usize, u32> {
    match target {
        Target::Light(light) if !(0x4000..0x4008).contains(&light) => Err(GL_INVALID_ENUM),
        Target::Material(face) if face != 0x0408 => Err(GL_INVALID_ENUM),
        _ => {
            let n = match target {
                Target::Light(_) => match pname {
                    AMBIENT | DIFFUSE | SPECULAR | POSITION => 4,
                    SPOT_DIRECTION => 3,
                    SPOT_EXPONENT
                    | SPOT_CUTOFF
                    | CONSTANT_ATTENUATION
                    | LINEAR_ATTENUATION
                    | QUADRATIC_ATTENUATION => 1,
                    _ => return Err(GL_INVALID_ENUM),
                },
                Target::Material(_) => match pname {
                    AMBIENT | DIFFUSE | SPECULAR | EMISSION | AMBIENT_AND_DIFFUSE => 4,
                    SHININESS => 1,
                    _ => return Err(GL_INVALID_ENUM),
                },
                Target::Model => match pname {
                    MODEL_AMBIENT => 4,
                    MODEL_TWO_SIDE => 1,
                    _ => return Err(GL_INVALID_ENUM),
                },
            };
            if scalar && n != 1 {
                Err(GL_INVALID_ENUM)
            } else {
                Ok(n)
            }
        }
    }
}

/// Inverse-transpose of the modelview's upper 3x3, in column-major layout.
/// Singular transforms have undefined normals; keep their output finite.
pub fn normal_matrix(m: &Matrix4) -> Matrix4 {
    let (a, b, c, d, e, f, g, h, i) = (m[0], m[4], m[8], m[1], m[5], m[9], m[2], m[6], m[10]);
    let co = [
        [e * i - f * h, f * g - d * i, d * h - e * g],
        [c * h - b * i, a * i - c * g, b * g - a * h],
        [b * f - c * e, c * d - a * f, a * e - b * d],
    ];
    let det = a * co[0][0] + b * co[0][1] + c * co[0][2];
    if !det.is_finite() || det.abs() < f32::MIN_POSITIVE {
        return IDENTITY;
    }
    let mut out = IDENTITY;
    for r in 0..3 {
        for c in 0..3 {
            out[c * 4 + r] = co[r][c] / det;
        }
    }
    out
}

impl LightingState {
    /// Evaluate the same per-vertex equation as the native lighting shader.
    pub fn shade(&self, n: [f32; 3], point: [f32; 3], color: [f32; 4]) -> [f32; 4] {
        let dot = |a: [f32; 3], b: [f32; 3]| a.iter().zip(b).map(|(a, b)| a * b).sum::<f32>();
        let unit = |v: [f32; 3]| {
            let length = dot(v, v).sqrt();
            if length > 0. {
                v.map(|x| x / length)
            } else {
                [0.; 3]
            }
        };
        let ma = if self.color_material {
            color
        } else {
            self.ambient
        };
        let md = if self.color_material {
            color
        } else {
            self.diffuse
        };
        let mut result =
            std::array::from_fn::<_, 3, _>(|i| self.emission[i] + self.model_ambient[i] * ma[i]);
        for light in self.lights.iter().filter(|l| l.enabled) {
            let delta = std::array::from_fn(|i| {
                if light.position[3] == 0. {
                    light.position[i]
                } else {
                    light.position[i] / light.position[3] - point[i]
                }
            });
            let distance = dot(delta, delta).sqrt();
            let l = unit(delta);
            let mut attenuation = if light.position[3] == 0. {
                1.
            } else {
                let k = light.attenuation;
                1. / (k[0] + k[1] * distance + k[2] * distance * distance).max(0.000001)
            };
            if light.spot_cutoff != 180. {
                let spot = dot(unit(light.spot_direction), l.map(|v| -v));
                if spot < light.spot_cutoff.to_radians().cos() {
                    attenuation = 0.;
                } else if light.spot_exponent > 0. {
                    attenuation *= spot.max(0.).powf(light.spot_exponent);
                }
            }
            let diffuse = dot(n, l).max(0.);
            let spec = if diffuse > 0. {
                let nh = dot(n, unit([l[0], l[1], l[2] + 1.])).max(0.);
                if self.shininess == 0. {
                    1.
                } else {
                    nh.powf(self.shininess)
                }
            } else {
                0.
            };
            for i in 0..3 {
                result[i] += attenuation
                    * (light.ambient[i] * ma[i]
                        + diffuse * light.diffuse[i] * md[i]
                        + spec * light.specular[i] * self.specular[i]);
            }
        }
        [
            result[0].clamp(0., 1.),
            result[1].clamp(0., 1.),
            result[2].clamp(0., 1.),
            md[3].clamp(0., 1.),
        ]
    }

    pub fn transform_normal(&self, matrix: &Matrix4, normal: [f32; 3]) -> [f32; 3] {
        let v = matrix::transform(matrix, [normal[0], normal[1], normal[2], 0.]);
        let mut n = [v[0], v[1], v[2]];
        let length = if self.normalize {
            n.iter().map(|x| x * x).sum::<f32>().sqrt()
        } else if self.rescale {
            (matrix[8] * matrix[8] + matrix[9] * matrix[9] + matrix[10] * matrix[10]).sqrt()
        } else {
            1.
        };
        if length > 0. {
            n = n.map(|x| x / length);
        }
        n
    }

    pub fn set(
        &mut self,
        target: Target,
        pname: u32,
        values: &[f32],
        mv: &Matrix4,
    ) -> Result<(), u32> {
        let n = parameter_count(target, pname, false)?;
        if values.len() != n || values.iter().any(|v| !v.is_finite()) {
            return Err(GL_INVALID_VALUE);
        }
        let scalar = values[0];
        let color = || [values[0], values[1], values[2], values[3]];
        match target {
            Target::Light(id) => {
                let light = &mut self.lights[(id - 0x4000) as usize];
                match pname {
                    AMBIENT => light.ambient = color(),
                    DIFFUSE => light.diffuse = color(),
                    SPECULAR => light.specular = color(),
                    POSITION => light.position = matrix::transform(mv, color()),
                    SPOT_DIRECTION => {
                        let d = matrix::transform(
                            &normal_matrix(mv),
                            [values[0], values[1], values[2], 0.],
                        );
                        light.spot_direction = [d[0], d[1], d[2]];
                    }
                    SPOT_EXPONENT => {
                        if !(0.0..=128.).contains(&scalar) {
                            return Err(GL_INVALID_VALUE);
                        }
                        light.spot_exponent = scalar;
                    }
                    SPOT_CUTOFF => {
                        if scalar != 180. && !(0.0..=90.).contains(&scalar) {
                            return Err(GL_INVALID_VALUE);
                        }
                        light.spot_cutoff = scalar;
                    }
                    CONSTANT_ATTENUATION | LINEAR_ATTENUATION | QUADRATIC_ATTENUATION => {
                        if scalar < 0. {
                            return Err(GL_INVALID_VALUE);
                        }
                        light.attenuation[(pname - CONSTANT_ATTENUATION) as usize] = scalar;
                    }
                    _ => unreachable!(),
                }
            }
            Target::Material(_) => match pname {
                AMBIENT => self.ambient = color(),
                DIFFUSE => self.diffuse = color(),
                SPECULAR => self.specular = color(),
                EMISSION => self.emission = color(),
                AMBIENT_AND_DIFFUSE => {
                    self.ambient = color();
                    self.diffuse = color();
                }
                SHININESS => {
                    if !(0.0..=128.).contains(&scalar) {
                        return Err(GL_INVALID_VALUE);
                    }
                    self.shininess = scalar;
                }
                _ => unreachable!(),
            },
            Target::Model => match pname {
                MODEL_AMBIENT => self.model_ambient = color(),
                MODEL_TWO_SIDE => self.two_sided = scalar != 0.,
                _ => unreachable!(),
            },
        }
        Ok(())
    }
    pub fn get(&self, target: Target, pname: u32) -> Result<Vec<f32>, u32> {
        parameter_count(target, pname, false)?;
        Ok(match target {
            Target::Light(id) => {
                let l = self.lights[(id - 0x4000) as usize];
                match pname {
                    AMBIENT => l.ambient.to_vec(),
                    DIFFUSE => l.diffuse.to_vec(),
                    SPECULAR => l.specular.to_vec(),
                    POSITION => l.position.to_vec(),
                    SPOT_DIRECTION => l.spot_direction.to_vec(),
                    SPOT_EXPONENT => vec![l.spot_exponent],
                    SPOT_CUTOFF => vec![l.spot_cutoff],
                    _ => vec![l.attenuation[(pname - CONSTANT_ATTENUATION) as usize]],
                }
            }
            Target::Material(_) => match pname {
                AMBIENT => self.ambient.to_vec(),
                DIFFUSE => self.diffuse.to_vec(),
                SPECULAR => self.specular.to_vec(),
                EMISSION => self.emission.to_vec(),
                SHININESS => vec![self.shininess],
                _ => return Err(GL_INVALID_ENUM),
            },
            Target::Model => match pname {
                MODEL_AMBIENT => self.model_ambient.to_vec(),
                _ => vec![self.two_sided as u8 as f32],
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_validation_and_eye_space_capture() {
        let mut s = LightingState::default();
        assert_eq!(s.lights[0].diffuse, [1.; 4]);
        assert_eq!(s.lights[1].diffuse, [0.; 4]);
        assert_eq!(
            s.set(Target::Light(0x4008), POSITION, &[0.; 4], &IDENTITY),
            Err(GL_INVALID_ENUM)
        );
        assert_eq!(
            s.set(Target::Light(0x4000), SPOT_CUTOFF, &[91.], &IDENTITY),
            Err(GL_INVALID_VALUE)
        );
        assert_eq!(s.lights[0].spot_cutoff, 180.);
        assert_eq!(
            s.set(Target::Material(0x0408), SHININESS, &[129.], &IDENTITY),
            Err(GL_INVALID_VALUE)
        );
        let mv = matrix::translate(1., 2., 3.);
        s.set(Target::Light(0x4000), POSITION, &[0., 0., 0., 1.], &mv)
            .unwrap();
        assert_eq!(s.lights[0].position, [1., 2., 3., 1.]);
        s.set(Target::Light(0x4000), POSITION, &[0., 0., 1., 0.], &mv)
            .unwrap();
        assert_eq!(s.lights[0].position, [0., 0., 1., 0.]);
        s.set(
            Target::Material(0x0408),
            AMBIENT_AND_DIFFUSE,
            &[0.1, 0.2, 0.3, 0.4],
            &IDENTITY,
        )
        .unwrap();
        assert_eq!(s.ambient, s.diffuse);
    }
    #[test]
    fn normals_use_inverse_transpose_and_remain_finite_when_singular() {
        let mv = matrix::multiply(&matrix::rotate(37., 0., 0., 1.), &matrix::scale(2., 3., 4.));
        let normal = normal_matrix(&mv);
        let transformed = matrix::transform(&normal, [1., 0., 0., 0.]);
        let reference = matrix::transform(&matrix::rotate(37., 0., 0., 1.), [0.5, 0., 0., 0.]);
        for i in 0..3 {
            assert!((transformed[i] - reference[i]).abs() < 0.00001);
        }
        assert!(normal_matrix(&matrix::scale(0., 0., 0.))
            .iter()
            .all(|v| v.is_finite()));
    }
}
