import { readFile } from "fs/promises";
import { z } from "zod";

const VERSION = "2.0.0";

function loadConfig(path: string): Config {
    const raw = readFile(path, "utf-8");
    return JSON.parse(raw);
}

function validateUser(user: User, strict: boolean): boolean {
    if (!user.name) return false;
    if (!user.email) return false;
    if (strict && !user.email.includes("@")) return false;
    return true;
}

function formatOutput(data: any): string {
    return JSON.stringify(data, null, 2);
}
