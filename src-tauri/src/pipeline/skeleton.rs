//! 阶段 4 前置：Zhang-Suen 骨架细化（对应 C++ ThinZhangSuen）
//! 阶段 3 的 RepairDirectionalGaps 也需要骨架化，故单独成模块

const MAXIMUM_SKELETON_ITERATIONS: usize = 192;

/// 8 邻域二进制跳变计数（对应 C++ BinaryTransitions）
fn binary_transitions(p: &[u8; 8]) -> i32 {
    let mut transitions = 0;
    for i in 0..8 {
        transitions += if p[i] == 0 && p[(i + 1) % 8] != 0 {
            1
        } else {
            0
        };
    }
    transitions
}

/// Zhang-Suen 细化（对应 C++ ThinZhangSuen）
pub fn thin_zhang_suen(image: &mut [u8], width: u32, height: u32) {
    if width < 3 || height < 3 {
        return;
    }
    let w = width as usize;
    let mut remove: Vec<usize> = Vec::with_capacity(image.len() / 16);

    for _ in 0..MAXIMUM_SKELETON_ITERATIONS {
        let mut changed = false;
        for substep in 0..2 {
            remove.clear();
            for y in 1..height as usize - 1 {
                for x in 1..w - 1 {
                    let index = y * w + x;
                    if image[index] == 0 {
                        continue;
                    }
                    let p = [
                        image[index - w],
                        image[index - w + 1],
                        image[index + 1],
                        image[index + w + 1],
                        image[index + w],
                        image[index + w - 1],
                        image[index - 1],
                        image[index - w - 1],
                    ];
                    let count = p.iter().map(|&v| v as i32).sum::<i32>();
                    if !(2..=6).contains(&count) || binary_transitions(&p) != 1 {
                        continue;
                    }
                    let first_triplet = if substep == 0 {
                        p[0] * p[2] * p[4] == 0
                    } else {
                        p[0] * p[2] * p[6] == 0
                    };
                    let second_triplet = if substep == 0 {
                        p[2] * p[4] * p[6] == 0
                    } else {
                        p[0] * p[4] * p[6] == 0
                    };
                    if first_triplet && second_triplet {
                        remove.push(index);
                    }
                }
            }
            for &index in &remove {
                image[index] = 0;
            }
            changed = changed || !remove.is_empty();
        }
        if !changed {
            break;
        }
    }
}

/// 骨架 8 邻点（对应 C++ PixelNeighbors）
/// 对角邻点仅在两个正交邻点都为空时计入（避免跨线粘连）
const NEIGHBOR_DELTAS: [(i32, i32); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

/// Compact eight-neighbor representation used by the tracing hot path.
/// A mask avoids one heap allocation per image pixel while preserving the
/// historical neighbor order returned by `pixel_neighbors`.
pub fn pixel_neighbor_mask(skeleton: &[u8], width: u32, height: u32, index: usize) -> u8 {
    if width == 0 || height == 0 || index >= skeleton.len() || skeleton[index] == 0 {
        return 0;
    }
    let width_i = width as i32;
    let height_i = height as i32;
    let x = (index % width as usize) as i32;
    let y = (index / width as usize) as i32;
    let mut mask = 0u8;
    for (direction, &(ox, oy)) in NEIGHBOR_DELTAS.iter().enumerate() {
        let nx = x + ox;
        let ny = y + oy;
        if nx < 0 || ny < 0 || nx >= width_i || ny >= height_i {
            continue;
        }
        let neighbor = (ny * width_i + nx) as usize;
        if skeleton[neighbor] == 0 {
            continue;
        }
        if ox != 0 && oy != 0 {
            let horizontal = (y * width_i + nx) as usize;
            let vertical = (ny * width_i + x) as usize;
            if skeleton[horizontal] != 0 || skeleton[vertical] != 0 {
                continue;
            }
        }
        mask |= 1 << direction;
    }
    mask
}

pub struct MaskedPixelNeighbors {
    mask: u8,
    width: usize,
    x: i32,
    y: i32,
    direction: usize,
}

impl Iterator for MaskedPixelNeighbors {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.direction < NEIGHBOR_DELTAS.len() {
            let direction = self.direction;
            self.direction += 1;
            if self.mask & (1 << direction) == 0 {
                continue;
            }
            let (ox, oy) = NEIGHBOR_DELTAS[direction];
            return Some(((self.y + oy) as usize) * self.width + (self.x + ox) as usize);
        }
        None
    }
}

pub fn masked_pixel_neighbors(mask: u8, width: u32, index: usize) -> MaskedPixelNeighbors {
    let width = width.max(1) as usize;
    let x = (index % width) as i32;
    let y = (index / width) as i32;
    MaskedPixelNeighbors {
        mask,
        width,
        x,
        y,
        direction: 0,
    }
}

pub fn pixel_neighbors(skeleton: &[u8], width: u32, height: u32, index: usize) -> Vec<usize> {
    let mask = pixel_neighbor_mask(skeleton, width, height, index);
    masked_pixel_neighbors(mask, width, index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_neighbor_mask_matches_expected_connectivity() {
        let skeleton = vec![1u8; 9];
        let center = pixel_neighbors(&skeleton, 3, 3, 4);
        assert_eq!(center, vec![1, 3, 5, 7]);

        let diagonal = vec![1u8, 0, 0, 0, 1u8, 0, 0, 0, 0];
        assert_eq!(pixel_neighbors(&diagonal, 3, 3, 0), vec![4]);
        assert_eq!(pixel_neighbors(&diagonal, 3, 3, 4), vec![0]);
    }
}
