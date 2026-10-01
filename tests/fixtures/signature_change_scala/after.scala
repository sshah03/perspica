package example

object Auth {
  def authenticate(username: String, password: String, requireMfa: Boolean = false): Boolean = {
    if (username.isEmpty || password.isEmpty) false
    else if (requireMfa && !checkMfa(username)) false
    else checkCredentials(username, password)
  }

  private def checkMfa(u: String): Boolean = u.startsWith("admin")

  private def checkCredentials(u: String, p: String): Boolean = u.nonEmpty && p.nonEmpty
}
