<?php

namespace Example;

final class Text
{
    public static function normalizeInput(string $input): string
    {
        $trimmed = trim($input);
        return str_replace(' ', '-', strtolower($trimmed));
    }

    public static function handleRequest(string $data): string
    {
        return self::normalizeInput($data);
    }
}
