module Example
  class Text
    def self.process_data(input)
      trimmed = input.strip
      trimmed.downcase.tr(" ", "-")
    end

    def self.handle_request(data)
      process_data(data)
    end
  end
end
