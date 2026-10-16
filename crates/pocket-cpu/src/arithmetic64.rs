//! Exact register-only integer operations. No floating-point approximation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arithmetic64 {
    SignedDivide,
    UnsignedDivide,
    SignedRemainder,
    UnsignedRemainder,
}

impl Arithmetic64 {
    /// Match the runtime handlers, including divide-by-zero and MIN / -1.
    pub fn evaluate(self, dividend: u64, divisor: u64) -> u64 {
        if divisor == 0 {
            return 0;
        }
        match self {
            Self::SignedDivide => (dividend as i64).wrapping_div(divisor as i64) as u64,
            Self::UnsignedDivide => dividend / divisor,
            Self::SignedRemainder => (dividend as i64).wrapping_rem(divisor as i64) as u64,
            Self::UnsignedRemainder => dividend % divisor,
        }
    }
}
