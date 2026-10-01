package example

object Text {
  def normalizeInput(input: String): String = {
    val trimmed = input.trim
    trimmed.toLowerCase.replace(" ", "-")
  }

  def handleRequest(data: Map[String, String]): Map[String, String] =
    Map("result" -> normalizeInput(data("input")))
}
