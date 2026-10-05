package example

import kotlin.collections.ArrayDeque
import java.time.Instant

object Log {
    fun lines(): ArrayDeque<String> = ArrayDeque(listOf(Instant.now().toString()))
}
