<?php

namespace Example;

class Auth
{
    public function authenticate(string $username, string $password, bool $requireMfa = false): bool
    {
        if ($username === '' || $password === '') {
            return false;
        }
        if ($requireMfa && !$this->checkMfa($username)) {
            return false;
        }
        return $this->checkCredentials($username, $password);
    }

    private function checkMfa(string $u): bool
    {
        return str_starts_with($u, 'admin');
    }

    private function checkCredentials(string $u, string $p): bool
    {
        return $u !== '' && $p !== '';
    }
}
