package example

object Text {
  def processData(input: String): String = {
    val trimmed = input.trim
    trimmed.toLowerCase.replace(" ", "-")
  }

  def handleRequest(data: Map[String, String]): Map[String, String] =
    Map("result" -> processData(data("input")))
}
