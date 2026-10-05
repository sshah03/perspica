package example

object Auth {
    fun authenticate(username: String, password: String): Boolean {
        if (username.isEmpty() || password.isEmpty()) return false
        return checkCredentials(username, password)
    }

    private fun checkCredentials(u: String, p: String): Boolean = u.isNotEmpty() && p.isNotEmpty()
}
