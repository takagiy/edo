import { readFile } from "fs";

/** A shape. */
export interface Shape {
  area(): number;
}

export type Id = string;

enum Color {
  Red,
}

export abstract class Base implements Shape {
  abstract area(): number;

  /** Describes. */
  describe(): string {
    return `area ${this.area()}`;
  }
}

export function make(id: Id): Base | undefined {
  if (id) {
    return undefined;
  }
}

export const handler = async (id: Id): Promise<void> => {
  await readFile(id);
};
