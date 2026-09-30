function processUserData(user: User): ProcessedUser {
    // Validate
    if (!user.name) throw new Error("Name required");
    if (!user.email) throw new Error("Email required");
    if (!user.email.includes("@")) throw new Error("Invalid email");

    // Transform
    const normalizedName = user.name.trim().toLowerCase();
    const normalizedEmail = user.email.trim().toLowerCase();
    const displayName = normalizedName.split(" ").map(w => w[0].toUpperCase() + w.slice(1)).join(" ");

    return {
        name: displayName,
        email: normalizedEmail,
        isValid: true,
    };
}
