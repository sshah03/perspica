import { debounce } from "lodash";
import { useState } from "react";

function SearchBox(): JSX.Element {
    const [query, setQuery] = useState("");
    const debouncedSearch = debounce((q: string) => {
        console.log("searching:", q);
    }, 300);
    return null;
}
