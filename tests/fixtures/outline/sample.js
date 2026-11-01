import fs from "fs";

/** Adds numbers. */
export function add(a, b) {
  return a + b;
}

export const twice = (x) => {
  return x * 2;
};

const inline = (x) => x + 1;

// A counter.
export default class Counter {
  constructor() {
    this.n = 0;
  }

  // Increments.
  inc() { this.n++; }
}

describe("x", () => {
  it("y", () => {});
});
