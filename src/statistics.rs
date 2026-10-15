//! 精确二项检验和配对 McNemar 检验，无外部依赖。

/// 返回 `X ~ Binomial(trials, p)` 时的 `P(X >= failures)`。
///
/// `p` 无效或非有限时返回 `NaN`。失败次数超过试验次数时返回零；
/// 失败次数阈值为零时返回一，即使试验次数也为零。
/// 小于 `f64` 可表示范围的概率可能舍入为零。
/// 超过 `2^53` 的计数转换为 `f64` 时会丢失整数精度。
pub fn binomial_upper_tail(trials: u64, failures: u64, p: f64) -> f64 {
    if !p.is_finite() || !(0.0..=1.0).contains(&p) {
        return f64::NAN;
    }
    if failures == 0 {
        return 1.0;
    }
    if failures > trials || p == 0.0 {
        return 0.0;
    }
    if p == 1.0 {
        return 1.0;
    }
    if failures == 1 {
        return -((trials as f64) * (-p).ln_1p()).exp_m1();
    }
    if failures == trials {
        return ((trials as f64) * p.ln()).exp();
    }

    // 尾概率为 I_p(failures, trials - failures + 1)。选择贝塔连分式
    // 中更利于收敛的一侧，在对数域计算。
    let a = failures as f64;
    let b = (trials - failures) as f64 + 1.0;
    let complement = p > (a + 1.0) / (a + b + 2.0);
    let (a, b, x) = if complement {
        (b, a, 1.0 - p)
    } else {
        (a, b, p)
    };
    let log_probability =
        (beta_log_prefactor(a, b, x) + beta_fraction(a, b, x).ln() - a.ln()).min(0.0);
    let probability = if complement {
        -log_probability.exp_m1()
    } else {
        log_probability.exp()
    };
    probability.clamp(0.0, 1.0)
}

/// 用单侧精确 McNemar 检验判断配对样本中目标的失败是否更多。
///
/// 仅结果不一致的样本对参与条件二项检验；没有不一致的样本对时返回一。
/// 总数溢出时返回 `NaN`，不会触发 panic。
pub fn exact_paired_p_value(target_only: u64, reference_only: u64) -> f64 {
    match target_only.checked_add(reference_only) {
        Some(discordant) => binomial_upper_tail(discordant, target_only, 0.5),
        None => f64::NAN,
    }
}

// 使用二项偏差公式计算 log(x^a (1-x)^b / B(a,b))。
// 直接相减对数伽马值时，大项会近乎完全抵消；这种写法在大参数分布的
// 众数附近仍能保持精度。
fn beta_log_prefactor(a: f64, b: f64, x: f64) -> f64 {
    let total = a + b;
    -deviance_part(a, total * x) - deviance_part(b, total * (1.0 - x)) + stirling_error(total)
        - stirling_error(a)
        - stirling_error(b)
        + 0.5 * (a.ln() + b.ln() - total.ln() - (2.0 * std::f64::consts::PI).ln())
}

// 计算 x*ln(x/mean) + mean-x，可能发生相消时改用级数求值。
fn deviance_part(x: f64, mean: f64) -> f64 {
    if (x - mean).abs() < 0.1 * (x + mean) {
        let v = (x - mean) / (x + mean);
        let mut sum = (x - mean) * v;
        let mut term = 2.0 * x * v;
        let squared = v * v;
        let mut denominator = 3.0;
        loop {
            term *= squared;
            let next = sum + term / denominator;
            if next == sum {
                return sum;
            }
            sum = next;
            denominator += 2.0;
        }
    }
    x * (x / mean).ln() + mean - x
}

// Stirling 对数阶乘近似的误差。所有调用方都传入正整数；
// 较小的参数只需对少量项精确求和。
fn stirling_error(x: f64) -> f64 {
    if x < 16.0 {
        let log_factorial: f64 = (2..=x as u32).map(|i| (i as f64).ln()).sum();
        return log_factorial - (x + 0.5) * x.ln() + x - 0.5 * (2.0 * std::f64::consts::PI).ln();
    }
    let inverse_square = 1.0 / (x * x);
    (1.0 / 12.0
        - inverse_square
            * (1.0 / 360.0
                - inverse_square
                    * (1.0 / 1260.0
                        - inverse_square
                            * (1.0 / 1680.0
                                - inverse_square
                                    * (1.0 / 1188.0
                                        - inverse_square
                                            * (691.0 / 360360.0
                                                - inverse_square * (1.0 / 156.0)))))))
        / x
}

// 使用修正的 Lentz 算法计算不完全贝塔函数的连分式。
fn beta_fraction(a: f64, b: f64, x: f64) -> f64 {
    fn nonzero(value: f64) -> f64 {
        if value.abs() < 1e-300 {
            1e-300_f64.copysign(value)
        } else {
            value
        }
    }

    let total = a + b;
    let mut c = 1.0;
    let mut d = 1.0 / nonzero(1.0 - total * x / (a + 1.0));
    let mut result = d;
    let mut m = 1.0;
    loop {
        let twice = 2.0 * m;
        let even = m * (b - m) * x / ((a - 1.0 + twice) * (a + twice));
        d = 1.0 / nonzero(1.0 + even * d);
        c = nonzero(1.0 + even / c);
        result *= d * c;

        let odd = -(a + m) * (total + m) * x / ((a + twice) * (a + 1.0 + twice));
        d = 1.0 / nonzero(1.0 + odd * d);
        c = nonzero(1.0 + odd / c);
        let change = d * c;
        result *= change;
        if (change - 1.0).abs() <= 8.0 * f64::EPSILON {
            return result;
        }
        m += 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(actual.is_finite(), "non-finite result: {actual}");
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn binomial_boundaries_and_invalid_inputs() {
        assert_eq!(binomial_upper_tail(0, 0, 0.5), 1.0);
        assert_eq!(binomial_upper_tail(0, 1, 0.5), 0.0);
        assert_eq!(binomial_upper_tail(10, 0, 0.0), 1.0);
        assert_eq!(binomial_upper_tail(10, 1, 0.0), 0.0);
        assert_eq!(binomial_upper_tail(10, 10, 1.0), 1.0);
        assert_eq!(binomial_upper_tail(10, 11, 1.0), 0.0);
        close(binomial_upper_tail(10, 10, 0.5), 1.0 / 1024.0, 1e-17);
        for p in [-0.1, 1.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(binomial_upper_tail(10, 3, p).is_nan());
        }
    }

    #[test]
    fn binomial_matches_independently_computed_probabilities() {
        // 参考值通过对二项概率质量进行 65 位十进制精度的求和得到。
        close(binomial_upper_tail(10, 3, 0.2), 0.322_200_473_6, 2e-14);
        close(binomial_upper_tail(10, 8, 0.8), 0.677_799_526_4, 2e-14);
        close(
            binomial_upper_tail(1000, 900, 0.8) / 8.731_206_511_212_329e-18,
            1.0,
            2e-12,
        );
    }

    #[test]
    fn large_binomial_does_not_start_from_an_underflowed_endpoint() {
        close(
            binomial_upper_tail(10_000, 5000, 0.5),
            0.503_989_323_069_691_1,
            2e-12,
        );
        close(
            binomial_upper_tail(10_000, 4900, 0.49),
            0.503_963_522_441_811_8,
            2e-12,
        );
        close(binomial_upper_tail(1_000_001, 500_001, 0.5), 0.5, 2e-11);
        // 此处若先计算 p^failures 会下溢，但完整尾概率仍可用次正规数表示。
        let tiny_tail = binomial_upper_tail(10_000, 10, 1e-35);
        assert!(tiny_tail > 0.0 && tiny_tail.is_finite());
        let small_p_tail = binomial_upper_tail(1_000_000, 1, 1e-20);
        close(small_p_tail / 1e-14, 1.0, 1e-13);
    }

    #[test]
    fn binomial_tails_are_monotonic_and_symmetric() {
        for p in [0.01, 0.2, 0.5, 0.8, 0.99] {
            let mut previous = 1.0;
            for k in 0..=81 {
                let tail = binomial_upper_tail(80, k, p);
                assert!(tail <= previous + 1e-14);
                previous = tail;
                if k > 0 && k <= 80 {
                    close(tail + binomial_upper_tail(80, 81 - k, 1.0 - p), 1.0, 5e-14);
                }
            }
        }
        let mut previous = 0.0;
        for step in 0..=100 {
            let tail = binomial_upper_tail(100, 37, step as f64 / 100.0);
            assert!(tail >= previous - 1e-14);
            previous = tail;
        }
    }

    #[test]
    fn paired_exact_test_uses_only_discordant_pairs_and_correct_direction() {
        assert_eq!(exact_paired_p_value(0, 0), 1.0);
        assert_eq!(exact_paired_p_value(0, 10), 1.0);
        close(exact_paired_p_value(5, 0), 0.03125, 1e-16);
        close(exact_paired_p_value(4, 1), 0.1875, 2e-15);
        close(exact_paired_p_value(3, 3), 0.65625, 3e-15);
        assert!(exact_paired_p_value(8, 2) < exact_paired_p_value(2, 8));
        assert!(exact_paired_p_value(u64::MAX, 1).is_nan());
    }
}
