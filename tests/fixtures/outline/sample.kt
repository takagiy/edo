package demo

import kotlin.math.sqrt

/** A data object. */
@Suppress("unused")
data object Foo {
  fun hoge() = do_hoge(1, 2)
  fun fuga() {
    hoge()
  }
}

@JvmInline
value class Id(val raw: String)

interface Shape {
  fun area(): Double
}

class Circle(private val r: Double) : Shape {
  // Area of the circle.
  override fun area(): Double {
    return Math.PI * r * r
  }

  companion object {
    fun unit() = Circle(1.0)
  }
}

typealias Name = String
