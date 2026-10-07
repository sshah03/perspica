class Auth
  def authenticate(username, password)
    return false if username.empty? || password.empty?

    check_credentials(username, password)
  end

  private

  def check_credentials(user, pass)
    !user.empty? && !pass.empty?
  end
end
