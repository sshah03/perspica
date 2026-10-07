module Example
  class Text
    def self.normalize_input(input)
      trimmed = input.strip
      trimmed.downcase.tr(" ", "-")
    end

    def self.handle_request(data)
      normalize_input(data)
    end
  end
end
