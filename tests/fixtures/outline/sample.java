package demo;

import java.util.List;

/** A shape. */
public interface Shape {
    double area();
}

@Deprecated
public class Circle implements Shape {
    private final double r;

    /** Creates a circle. */
    public Circle(double r) {
        this.r = r;
    }

    @Override
    public double area() {
        return Math.PI * r * r;
    }

    static class Inner {
        void run() { System.out.println("x"); }
    }

    enum Kind { A, B }
}
