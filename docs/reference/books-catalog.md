# Packt Library Catalog (82 books)

Catalog of the Packt Publishing PDF library stored at
`scratchpad/libros/Libros/`. Each PDF is named only by its ISBN-13. Titles and
authors were resolved from OpenLibrary and web sources.

Relevance is scored against the **infinigpu** project: a 100%-custom GPU
virtualization driver in Rust — a QEMU virtual GPU device on a Linux host plus
guest WDDM/DRM drivers, sharing NVIDIA GPUs across many Windows/Linux VMs.

- **HIGH** = OS/kernel internals, device drivers, GPU/graphics (OpenGL/Vulkan),
  Rust systems programming, QEMU/KVM/virtualization, embedded Linux, C/C++ systems.
- **MED** = adjacent systems/security/graphics-tooling with partial overlap.
- **LOW** = web, data science, ML/AI, business, general app dev.

All 82 ISBNs were resolved.

---

## MUST-READ for infinigpu (HIGH-relevance shortlist)

Sorted by relevance to the project (implementation language → platform → driver
craft → kernel → GPU/graphics → embedded).

| Filename | Title | Author | Why relevant |
|----------|-------|--------|--------------|
| `9781838828103.pdf` | Complete Rust Programming Reference Guide | Rahul Sharma, Vesa Kaihlavirta, Claus Matzinger | Rust is infinigpu's implementation language — systems-level Rust patterns, concurrency, and low-level constructs. |
| `9781838828714.pdf` | Mastering KVM Virtualization (2nd Ed) | Vedran Dakic, Humble Devassy Chirammal, Prasad Mukhedkar, Anil Vettathu | The exact platform: QEMU/KVM VM design, virtio, device passthrough — the host side infinigpu plugs into. |
| `9781789342048.pdf` | Mastering Linux Device Driver Development | John Madieu | Advanced Linux drivers: PCI, DMA, interrupts, memory mapping — core skills for the host GPU device model. |
| `9781803240060.pdf` | Linux Device Driver Development | John Madieu | Foundational Linux char/PCI driver development for kernel 4.x/5.x — the guest DRM-side driver craft. |
| `9781838558802.pdf` | Linux Device Driver Development Cookbook | Rodolfo Giometti | Recipe-style driver tasks (char devices, ioctl, mmap, sysfs) for embedded Linux — directly applicable. |
| `9781801079518.pdf` | Linux Kernel Programming Part 2 — Char Device Drivers and Kernel Synchronization | Kaiwan N. Billimoria | User↔kernel interfaces, peripheral I/O, hardware interrupts, locking — the interface layer of the guest driver. |
| `9781789953435.pdf` | Linux Kernel Programming | Kaiwan N. Billimoria | Kernel internals, modules, memory management, synchronization — baseline for any kernel-mode component. |
| `9781803244792.pdf` | Mastering Graphics Programming with Vulkan | Marco Castorina, Gabriel Sassone | Modern rendering engine from first principles — the GPU workload/pipeline the virtual device must serve. |
| `9781838986193.pdf` | 3D Graphics Rendering Cookbook (OpenGL and Vulkan) | Sergey Kosarevsky, Viktor Latypov | OpenGL/Vulkan rendering algorithms — understanding guest graphics APIs the driver forwards to the GPU. |
| `9781804615065.pdf` | Embedded Linux Development Using Yocto Project (3rd Ed) | Otavio Salvador, Daiane Angolini | Building custom Linux images/BSPs and kernel integration — useful for guest images and driver packaging. |

---

## Full catalog (all 82)

| ISBN / Filename | Title | Author | Relevance | Topic |
|-----------------|-------|--------|-----------|-------|
| `9781782161400.pdf` | Building Machine Learning Systems with Python | Willi Richert, Luis Pedro Coelho | LOW | ML / Python |
| `9781782162148.pdf` | Machine Learning with R | Brett Lantz | LOW | ML / R |
| `9781782162742.pdf` | Linux Shell Scripting Cookbook (2nd Ed) | Shantanu Tushar, Sarath Lakshman | MED | Linux shell / scripting |
| `9781782163367.pdf` | Python Data Visualization Cookbook | Igor Milovanovic | LOW | Data visualization |
| `9781782167167.pdf` | Android Security Cookbook | Keith Makan, Scott Alexander-Bown | MED | Mobile / security |
| `9781782173434.pdf` | Learning Big Data with Amazon Elastic MapReduce | Amarkant Singh, Vijay Rayapati | LOW | Big data / cloud |
| `9781782176428.pdf` | Java EE 7 Performance Tuning and Optimization | Osama Oransa | LOW | Java EE |
| `9781783000340.pdf` | Implementing Lean Six Sigma in 30 Days | Gopal Ranjan, Tanmay Vora | LOW | Business / process |
| `9781783001422.pdf` | StartupPro: How to Set Up and Grow a Tech Business | Martin Zwilling | LOW | Business |
| `9781783280414.pdf` | Kali Linux Wireless Penetration Testing (Beginner's Guide) | Vivek Ramachandran, Cameron Buchanan | MED | Security / wireless |
| `9781783280995.pdf` | Practical Data Analysis | Hector Cuesta | LOW | Data analysis |
| `9781783283576.pdf` | Backbone.js Patterns and Best Practices | Swarnendu De | LOW | Web / JS |
| `9781783284672.pdf` | Natural Language Processing with Java and LingPipe Cookbook | Breck Baldwin, Krishna Dayanidhi | LOW | NLP |
| `9781783287314.pdf` | Node.js Design Patterns | Mario Casciaro | LOW | Web / Node.js |
| `9781783287796.pdf` | Mastering Eclipse Plug-In Development | Alex Blewitt | LOW | Java / tooling |
| `9781783550654.pdf` | Twilio Cookbook (2nd Ed) | Roger Stringer | LOW | Telephony API |
| `9781783553358.pdf` | Python Data Analysis | Ivan Idris | LOW | Data analysis |
| `9781783554713.pdf` | ROS Robotics Projects | Lentin Joseph | MED | Robotics / C++ / hardware |
| `9781783554751.pdf` | PhoneGap for Enterprise | Kerri Shotts | LOW | Mobile / web |
| `9781783558414.pdf` | SketchUp 2014 for Architectural Visualization (2nd Ed) | Thomas Bleicher, Robin de Jongh | LOW | 3D modeling (tool use) |
| `9781783559602.pdf` | Building a Home Security System with BeagleBone | Bill Pretty | MED | Embedded / hardware |
| `9781783981267.pdf` | WebRTC Integrator's Guide | Altanai | LOW | Web / real-time media |
| `9781783981922.pdf` | PhantomJS Cookbook | Rob Friesel | LOW | Web / testing |
| `9781783983285.pdf` | MEAN Web Development | Amos Q. Haviv | LOW | Web / full-stack |
| `9781783983520.pdf` | Java EE 7 Development with NetBeans 8 | David R. Heffelfinger | LOW | Java EE |
| `9781783988365.pdf` | Mastering Machine Learning with scikit-learn | Gavin Hackeling | LOW | ML |
| `9781784390860.pdf` | R for Data Science | Dan Toomey | LOW | Data science / R |
| `9781784396572.pdf` | Learning C++ by Creating Games with UE4 | William Sherif | MED | C++ / game engine (intro) |
| `9781784396992.pdf` | Functional Python Programming | Steven Lott | LOW | Python |
| `9781784398637.pdf` | Advanced Machine Learning with Python | John Hearty | LOW | ML |
| `9781784398781.pdf` | Python 3 Object-Oriented Programming (2nd Ed) | Dusty Phillips | LOW | Python |
| `9781784399689.pdf` | Practical Machine Learning | Sunila Gollapudi | LOW | ML |
| `9781785280832.pdf` | TypeScript Design Patterns | Vilic Vane | LOW | Web / TS |
| `9781785281099.pdf` | Bootstrap Site Blueprints Volume II | Matt Lambert | LOW | Web / CSS |
| `9781785285073.pdf` | Blender 3D by Example | Romain Caudron, Pierre-Armand Nicq | LOW | 3D modeling (tool use) |
| `9781785285240.pdf` | Unity 5.x Shaders and Effects Cookbook | Alan Zucconi, Kenneth Lammers | MED | GPU shaders (high-level) |
| `9781785882074.pdf` | Learning Angular 2 | Pablo Deeleman | LOW | Web / Angular |
| `9781785884221.pdf` | Mastering Android Application Development | Antonio Pachon Ruiz | LOW | Mobile |
| `9781786462169.pdf` | TensorFlow Machine Learning Cookbook | Nick McClure | LOW | ML |
| `9781786463982.pdf` | Mastering OpenStack (2nd Ed) | Omar Khedher, Chandan Dutta Chowdhury | MED | Cloud / virtualization infra |
| `9781786469946.pdf` | Learning Vue.js 2 | Olga Filipova | LOW | Web / Vue |
| `9781787128422.pdf` | Deep Learning with Keras | Antonio Gulli, Sujit Pal | LOW | ML / deep learning |
| `9781788398763.pdf` | OpenStack Cloud Computing Cookbook (4th Ed) | Kevin Jackson, Cody Bunch, Egle Sigler, James Denton | MED | Cloud / virtualization infra |
| `9781788624060.pdf` | Software Architect's Handbook | Joseph Ingeno | LOW | Software architecture |
| `9781789342048.pdf` | Mastering Linux Device Driver Development | John Madieu | HIGH | Linux device drivers |
| `9781789342093.pdf` | Keras Reinforcement Learning Projects | Giuseppe Ciaburro | LOW | ML / RL |
| `9781789342529.pdf` | Linux Administration Cookbook | Adam K. Dean | MED | Linux sysadmin |
| `9781789534498.pdf` | CompTIA Project+ Certification Guide | J. Ashley Hunt | LOW | Certification / PM |
| `9781789953435.pdf` | Linux Kernel Programming | Kaiwan N. Billimoria | HIGH | Kernel internals / modules |
| `9781800564732.pdf` | Mastering TypeScript (4th Ed) | Nathan Rozentals | LOW | Web / TS |
| `9781801079518.pdf` | Linux Kernel Programming Part 2 — Char Device Drivers and Kernel Synchronization | Kaiwan N. Billimoria | HIGH | Char device drivers / kernel |
| `9781803230054.pdf` | Cybersecurity Strategies and Best Practices | Milad Aslaner | LOW | Security (enterprise strategy) |
| `9781803237916.pdf` | Web Applications Architecture Handbook | Mihaela Roxana Ghidersa | LOW | Web architecture |
| `9781803240060.pdf` | Linux Device Driver Development | John Madieu | HIGH | Linux device drivers |
| `9781803244792.pdf` | Mastering Graphics Programming with Vulkan | Marco Castorina, Gabriel Sassone | HIGH | GPU / Vulkan / rendering |
| `9781804615065.pdf` | Embedded Linux Development Using Yocto Project (3rd Ed) | Otavio Salvador, Daiane Angolini | HIGH | Embedded Linux / BSP |
| `9781805128724.pdf` | Transformers for NLP and Computer Vision (3rd Ed) | Denis Rothman | LOW | ML / AI |
| `9781835083468.pdf` | Generative AI with LangChain | Ben Auffarth | LOW | AI / LLM |
| `9781835083833.pdf` | Unlocking the Secrets of Prompt Engineering | Gilbert Mizrahi, Daniel Serfaty | LOW | AI / prompting |
| `9781835466384.pdf` | Modern Python Cookbook (Python 3.12) | Steven F. Lott | LOW | Python |
| `9781835467145.pdf` | GraphQL Best Practices | Artur Czemiel | LOW | Web / API |
| `9781835883228.pdf` | TypeScript 5 Design Patterns and Best Practices | Theofanis Despoudis | LOW | Web / TS |
| `9781836202271.pdf` | React Key Concepts | Maximilian Schwarzmüller | LOW | Web / React |
| `9781836207030.pdf` | LLM Design Patterns | Ken Huang | LOW | AI / LLM |
| `9781837027873 (1).pdf` | Mathematics of Machine Learning | Tivadar Danka | LOW | Math / ML |
| `9781837633289.pdf` | SQL Query Design Patterns and Best Practices | Steve Hughes, Dennis Neer, Ram Babu Singh, Shabbir H. Mala, Leslie Andrews, Chi Zhang | LOW | Database / SQL |
| `9781837633784.pdf` | Mastering Transformers | Savas Yildirim, Meysam Asgari-Chenaghlu | LOW | ML / NLP |
| `9781837637959.pdf` | Modern Full-Stack React Projects | Daniel Bugl | LOW | Web / full-stack |
| `9781838558802.pdf` | Linux Device Driver Development Cookbook | Rodolfo Giometti | HIGH | Linux device drivers (embedded) |
| `9781838822477.pdf` | The Complete Metasploit Guide | Sagar Rahalkar, Nipun Jaswal | MED | Security / penetration testing |
| `9781838828103.pdf` | Complete Rust Programming Reference Guide | Rahul Sharma, Vesa Kaihlavirta, Claus Matzinger | HIGH | Rust systems programming |
| `9781838828714.pdf` | Mastering KVM Virtualization (2nd Ed) | Vedran Dakic, Humble Devassy Chirammal, Prasad Mukhedkar, Anil Vettathu | HIGH | QEMU / KVM / virtualization |
| `9781838986193.pdf` | 3D Graphics Rendering Cookbook (OpenGL and Vulkan) | Sergey Kosarevsky, Viktor Latypov | HIGH | GPU / OpenGL / Vulkan |
| `9781838987572.pdf` | Node.js Web Development (5th Ed) | David Herron | LOW | Web / Node.js |
| `9781839215643.pdf` | SQL Injection Strategies | Ettore Galluccio, Edoardo Caselli, Gabriele Lombari | MED | Security / SQL injection |
| `9781849510301.pdf` | PostgreSQL 9.0 High Performance | Gregory Smith | LOW | Database / performance |
| `9781849685245.pdf` | Microsoft SQL Server 2012 Integration Services: An Expert Cookbook | Reza Rad | LOW | Database / ETL |
| `9781849685986.pdf` | Salesforce.com Customization Handbook | Rakesh Gupta, Sagar Pareek | LOW | CRM |
| `9781849693127.pdf` | Object-Oriented JavaScript | Stoyan Stefanov | LOW | Web / JS |
| `9781849693608.pdf` | Mobile Security: How to Secure, Privatize, and Recover Your Devices | Timothy Speed, Darla Nykamp, Mari Heiser, Joseph Anderson, Jaya Nampalli | LOW | Mobile / security |
| `9781849695107.pdf` | Penetration Testing with the Bash Shell | Keith Makan | MED | Security / Linux CLI |
| `9781849699792.pdf` | WebGL Game Development | Sumeet Arora | MED | GPU / WebGL graphics (high-level) |

---

## Notes

- `9781837027873` is stored on disk as **`9781837027873 (1).pdf`** (the parenthetical suffix is part of the actual filename).
- Resolution sources: OpenLibrary `/api/books` (75 books) and web search of Packt/AbeBooks/eBay listings (7 newer titles OpenLibrary did not have: 9781804615065, 9781805128724, 9781835467145, 9781836207030, 9781837027873, 9781837633289, 9781838828714).
- The library is overwhelmingly web-dev, data-science, and ML titles; only 10 books are directly on-topic for a systems-level GPU virtualization driver.
