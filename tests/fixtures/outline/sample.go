package main

import "fmt"

// Point is a point.
type Point struct {
	X int
}

type Shape interface {
	Area() float64
}

// Norm returns the norm.
func (p *Point) Norm() int {
	return p.X
}

func main() {
	fmt.Println(Point{X: 1}.Norm())
}

func one() int { return 1 }
