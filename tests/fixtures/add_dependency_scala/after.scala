package example

import scala.collection.mutable.ListBuffer
import java.time.Instant

object Log {
  def lines(): ListBuffer[String] = ListBuffer(Instant.now().toString)
}
