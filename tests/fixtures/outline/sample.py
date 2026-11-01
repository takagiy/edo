import os


# A helper.
def helper(x):
    return x + 1


@dataclass
class Point:
    """A point."""

    x: int

    def norm(self) -> float:
        """Returns the norm.

        Long description.
        """
        total = self.x * self.x
        return total ** 0.5

    @staticmethod
    def zero(): return Point(0)


def main():
    def inner():
        pass
    inner()
