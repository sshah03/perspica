package example

object Auth {
    fun authenticate(username: String, password: String, requireMfa: Boolean = false): Boolean {
        if (username.isEmpty() || password.isEmpty()) return false
        if (requireMfa && !checkMfa(username)) return false
        return checkCredentials(username, password)
    }

    private fun checkMfa(u: String): Boolean = u.startsWith("admin")

    private fun checkCredentials(u: String, p: String): Boolean = u.isNotEmpty() && p.isNotEmpty()
}
