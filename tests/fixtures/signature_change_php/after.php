<?php

namespace Example;

class Auth
{
    public function authenticate(string $username, string $password): bool
    {
        if ($username === '' || $password === '') {
            return false;
        }
        return $this->checkCredentials($username, $password);
    }

    private function checkCredentials(string $u, string $p): bool
    {
        return $u !== '' && $p !== '';
    }
}
