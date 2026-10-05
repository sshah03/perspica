package example

object Text {
    fun normalizeInput(input: String): String {
        val trimmed = input.trim()
        return trimmed.lowercase().replace(" ", "-")
    }

    fun handleRequest(data: Map<String, String>): Map<String, String> =
        mapOf("result" to normalizeInput(data.getValue("input")))
}
