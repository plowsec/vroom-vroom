# Generate the block portion of a vroom HEVD harness.
# @category vroom

import json

from ghidra.program.model.block import BasicBlockModel


# Set this to the physical GPA corresponding to the driver's image base in the
# paused checkpoint. Ghidra knows virtual addresses; vroom deliberately keeps
# physical translation explicit so a stale page table cannot silently patch a
# different page.
GPA_IMAGE_BASE = 0

program = getCurrentProgram()
name = program.getName()
image_base = program.getImageBase().getOffset()
blocks = BasicBlockModel(program).getCodeBlocks(monitor)
rows = []
next_id = 1

while blocks.hasNext():
    block = blocks.next()
    va = block.getMinAddress().getOffset()
    end = block.getMaxAddress().getOffset()
    rows.append({
        "id": next_id,
        "gpa": GPA_IMAGE_BASE + (va - image_base),
        "va": va,
        "size": end - va + 1,
        "module": name,
        "function": "",
        "source": None,
    })
    next_id += 1

output = askFile("Write vroom coverage blocks", "Write")
with open(output.getAbsolutePath(), "w", encoding="utf-8") as stream:
    json.dump(rows, stream, indent=2)
