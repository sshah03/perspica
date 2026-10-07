require "logger"
require "time"

class Log
  def lines
    Logger.new($stdout).tap { |l| l.info(Time.now.iso8601) }
  end
end
