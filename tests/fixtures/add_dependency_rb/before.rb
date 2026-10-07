require "logger"

class Log
  def lines
    Logger.new($stdout)
  end
end
