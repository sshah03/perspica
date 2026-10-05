package example

object Text {
    fun processData(input: String): String {
        val trimmed = input.trim()
        return trimmed.lowercase().replace(" ", "-")
    }

    fun handleRequest(data: Map<String, String>): Map<String, String> =
        mapOf("result" to processData(data.getValue("input")))
}
