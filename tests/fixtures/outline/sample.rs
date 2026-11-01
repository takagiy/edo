use std::fmt;

/// A point.
#[derive(Debug)]
pub struct Point {
    x: i32,
}

// Not a doc: separated by a blank line.

impl Point {
    /// Creates a point.
    pub fn new(x: i32) -> Self {
        Point { x }
    }

    fn get(&self) -> i32 { self.x }
}

pub trait Shape {
    fn area(&self) -> f64;
    fn name(&self) -> String {
        "shape".into()
    }
}

mod inner {
    pub enum Kind {
        A,
    }

    pub fn run() {
        let f = |x: i32| {
            x + 1
        };
        f(1);
    }
}

const MAX: usize = 10;
