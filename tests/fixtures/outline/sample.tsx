import React from "react";

type Props = { name: string };

/** Greets. */
export function Hello({ name }: Props) {
  return <div className="hello">Hello {name}</div>;
}

export const App = () => {
  return (
    <main>
      <Hello name="x" />
    </main>
  );
};
