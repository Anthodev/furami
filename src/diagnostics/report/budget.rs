//! Bounded diagnostic work before materialization, independent of private input size.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectionOversized;

pub(crate) struct ProjectionBudget {
    remaining: usize,
}
impl ProjectionBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }
    pub(crate) fn charge(&mut self, bytes: usize) -> Result<(), ProjectionOversized> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or(ProjectionOversized)?;
        Ok(())
    }
    pub(crate) fn collection(
        &mut self,
        count: usize,
        item_bytes: usize,
    ) -> Result<(), ProjectionOversized> {
        self.charge(count.checked_mul(item_bytes).ok_or(ProjectionOversized)?)
    }
}
