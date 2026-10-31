//! Wire-format lock tests: the serde tag vocabulary, field shapes, and
//! backward compatibility of this crate's types are pinned here so any
//! rename or shape change is an explicit, reviewed breaking change.

mod evolution;
mod round_trips;
mod shapes;
mod tags;
