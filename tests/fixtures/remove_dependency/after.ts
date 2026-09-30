import { useState, useDeferredValue } from "react";

function SearchBox(): JSX.Element {
    const [query, setQuery] = useState("");
    const deferredQuery = useDeferredValue(query);
    console.log("searching:", deferredQuery);
    return null;
}
