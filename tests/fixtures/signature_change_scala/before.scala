package example

object Auth {
  def authenticate(username: String, password: String): Boolean = {
    if (username.isEmpty || password.isEmpty) false
    else checkCredentials(username, password)
  }

  private def checkCredentials(u: String, p: String): Boolean = u.nonEmpty && p.nonEmpty
}
