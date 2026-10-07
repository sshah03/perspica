class Auth
  def authenticate(username, password, require_mfa: false)
    return false if username.empty? || password.empty?
    return false if require_mfa && !check_mfa(username)

    check_credentials(username, password)
  end

  private

  def check_mfa(user)
    user.start_with?("admin")
  end

  def check_credentials(user, pass)
    !user.empty? && !pass.empty?
  end
end
