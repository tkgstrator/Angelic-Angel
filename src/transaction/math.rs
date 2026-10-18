//! JavaScript と同じ結果になる数値処理と 3 次ベジェ曲線。

/// 16 進変換ループの上限。有限の入力では 14 桁程度で止まる。
const HEX_LOOP_LIMIT: usize = 64;

// ---- JavaScript 互換の数値処理 ---------------------------------------------------------------

/// `Math.sign`。±0 は符号を保ち、NaN は NaN を返す。
fn js_sign(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 {
        x
    } else if x > 0.0 {
        1.0
    } else {
        -1.0
    }
}

/// `Math.round`。半分は +∞ 側へ丸め、-0.5 以上 0 未満は -0 になる。
pub(super) fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let floor = x.floor();
    let rounded = if x - floor >= 0.5 { floor + 1.0 } else { floor };
    if rounded == 0.0 && x < 0.0 {
        -0.0
    } else {
        rounded
    }
}

/// 上流の `customRound`。
pub(super) fn custom_round(num: f64) -> f64 {
    let x = num.floor();
    if num - x >= 0.5 {
        num.ceil()
    } else {
        js_sign(num) * x.abs()
    }
}

pub(super) fn is_odd(index: usize) -> f64 {
    if index % 2 == 1 { -1.0 } else { 0.0 }
}

pub(super) fn interpolate(from: f64, to: f64, factor: f64) -> f64 {
    from * (1.0 - factor) + to * factor
}

fn hex_digit(n: f64) -> char {
    let n = n as u8;
    if n > 9 {
        char::from(n.saturating_add(55))
    } else {
        char::from(b'0' + n)
    }
}

/// 上流の `floatToHex`。整数部と小数部を 16 進で表す。
pub(super) fn float_to_hex(x: f64) -> String {
    let mut int_digits = Vec::new();
    let mut quotient = x.trunc();
    let mut current = x;
    let mut fraction = x - quotient;

    for _ in 0..HEX_LOOP_LIMIT {
        if quotient <= 0.0 {
            break;
        }
        let new_quotient = (current / 16.0).trunc();
        let remainder = (current - new_quotient * 16.0).trunc();
        int_digits.push(hex_digit(remainder));
        current = new_quotient;
        quotient = new_quotient;
    }
    let mut result: String = int_digits.into_iter().rev().collect();

    if fraction == 0.0 {
        return result;
    }
    result.push('.');
    for _ in 0..HEX_LOOP_LIMIT {
        if fraction <= 0.0 {
            break;
        }
        fraction *= 16.0;
        let integer = fraction.trunc();
        fraction -= integer;
        result.push(hex_digit(integer));
    }
    result
}

// ---- Cubic Bezier ----------------------------------------------------------------------------

pub(super) struct Cubic(pub(super) [f64; 4]);

impl Cubic {
    pub(super) fn value(&self, time: f64) -> f64 {
        if time <= 0.0 {
            self.start_gradient() * time
        } else if time >= 1.0 {
            1.0 + self.end_gradient() * (time - 1.0)
        } else {
            self.binary_search(time)
        }
    }

    fn start_gradient(&self) -> f64 {
        let c = &self.0;
        if c[0] > 0.0 {
            c[1] / c[0]
        } else if c[1] == 0.0 && c[2] > 0.0 {
            c[3] / c[2]
        } else {
            0.0
        }
    }

    fn end_gradient(&self) -> f64 {
        let c = &self.0;
        if c[2] < 1.0 {
            (c[3] - 1.0) / (c[2] - 1.0)
        } else if c[2] == 1.0 && c[0] < 1.0 {
            (c[1] - 1.0) / (c[0] - 1.0)
        } else {
            0.0
        }
    }

    fn binary_search(&self, time: f64) -> f64 {
        let c = &self.0;
        let (mut start, mut end, mut mid) = (0.0_f64, 1.0_f64, 0.0_f64);
        for _ in 0..100 {
            if start >= end {
                break;
            }
            mid = (start + end) / 2.0;
            let x_est = Self::calculate(c[0], c[2], mid);
            if (time - x_est).abs() < 0.00001 {
                return Self::calculate(c[1], c[3], mid);
            }
            if x_est < time {
                start = mid;
            } else {
                end = mid;
            }
        }
        Self::calculate(c[1], c[3], mid)
    }

    fn calculate(a: f64, b: f64, m: f64) -> f64 {
        let one_minus_m = 1.0 - m;
        3.0 * a * one_minus_m * one_minus_m * m + 3.0 * b * one_minus_m * m * m + m * m * m
    }
}
