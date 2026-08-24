//! Generic lane-operation helpers live with the supervisor implementation.
//!
//! Keeping this module explicit makes the one-lane-at-a-time claim a lane
//! invariant instead of a property accidentally attached to the root host.
